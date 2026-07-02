use std::collections::{BTreeMap, HashMap};
use std::sync::{Mutex, OnceLock};

use reqwest::{Client, RequestBuilder};
use serde_json::{json, Map, Value};
use tokio::runtime::Runtime;

use crate::abi::{self, IrodoriConnectorBuffer};
use crate::{ABI_VERSION, CONFIG_JSON, DRIVER_LINKED, ENGINE, MANIFEST_JSON};

static CONNECTIONS: OnceLock<Mutex<HashMap<String, TrinoConnection>>> = OnceLock::new();
static RUNTIME: OnceLock<Runtime> = OnceLock::new();

#[derive(Clone)]
struct TrinoConnection {
    client: Client,
    config: TrinoConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrinoConfig {
    base_url: String,
    user: String,
    password: Option<String>,
    bearer_token: Option<String>,
    catalog: Option<String>,
    schema: Option<String>,
    redaction_values: Vec<String>,
}

#[derive(Default)]
struct ObjectMeta {
    schema: String,
    name: String,
    columns: Vec<Value>,
}

type QueryRows = Vec<Vec<Value>>;
type QueryOutput = (Vec<String>, QueryRows, bool);

fn connections() -> &'static Mutex<HashMap<String, TrinoConnection>> {
    CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn runtime() -> Result<&'static Runtime, String> {
    if let Some(runtime) = RUNTIME.get() {
        return Ok(runtime);
    }
    let runtime = Runtime::new().map_err(|err| format!("create tokio runtime failed: {err}"))?;
    let _ = RUNTIME.set(runtime);
    RUNTIME
        .get()
        .ok_or_else(|| "create tokio runtime failed.".to_string())
}

pub fn call_json(request: IrodoriConnectorBuffer) -> IrodoriConnectorBuffer {
    let request = match abi::parse_request(request) {
        Ok(request) => request,
        Err(response) => return response,
    };
    let method = match abi::request_method(request.as_ref()) {
        Ok(method) => method,
        Err(response) => return response,
    };

    match method {
        "health" | "ping" => abi::ok(Map::from_iter([
            ("engine".to_string(), Value::String(ENGINE.to_string())),
            ("abiVersion".to_string(), json!(ABI_VERSION)),
            ("driverLinked".to_string(), Value::Bool(DRIVER_LINKED)),
        ])),
        "describe" | "capabilities" => abi::ok(Map::from_iter([
            ("engine".to_string(), Value::String(ENGINE.to_string())),
            ("abiVersion".to_string(), json!(ABI_VERSION)),
            ("driverLinked".to_string(), Value::Bool(DRIVER_LINKED)),
            (
                "manifest".to_string(),
                serde_json::from_str(MANIFEST_JSON).unwrap_or(Value::Null),
            ),
            (
                "config".to_string(),
                serde_json::from_str(CONFIG_JSON).unwrap_or(Value::Null),
            ),
        ])),
        "manifest" => abi::owned_buffer(MANIFEST_JSON.to_string()),
        "config" => abi::owned_buffer(CONFIG_JSON.to_string()),
        "connect" => connect(request.as_ref().expect("connect has request")),
        "query" => query(request.as_ref().expect("query has request")),
        "metadata" => metadata(request.as_ref().expect("metadata has request")),
        "close" => close(request.as_ref().expect("close has request")),
        other => abi::error(
            "connector.unknownMethod",
            format!("unknown connector method: {other}"),
        ),
    }
}

fn connect(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let config = match TrinoConfig::from_request(request) {
        Ok(config) => config,
        Err(err) => return abi::error("connector.invalidRequest", err),
    };
    let connection = TrinoConnection {
        client: Client::new(),
        config,
    };
    let version = match runtime().and_then(|runtime| runtime.block_on(load_version(&connection))) {
        Ok(version) => version,
        Err(err) => return abi::error("connector.connectFailed", connection.config.redact(&err)),
    };
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let response = Map::from_iter([
        ("engine".to_string(), Value::String(ENGINE.to_string())),
        (
            "connectionId".to_string(),
            Value::String(connection_id.clone()),
        ),
        ("driverLinked".to_string(), Value::Bool(DRIVER_LINKED)),
        (
            "endpoint".to_string(),
            Value::String(connection.config.base_url.clone()),
        ),
        ("serverVersion".to_string(), Value::String(version)),
    ]);
    guard.insert(connection_id, connection);
    abi::ok(response)
}

fn query(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let Some(sql) = abi::string_field(request, "sql")
        .or_else(|| abi::string_field(request, "query"))
        .or_else(|| abi::string_field(request, "statement"))
    else {
        return abi::error(
            "connector.invalidRequest",
            "query requires a string sql, query, or statement field.",
        );
    };
    let connection = match connection(&connection_id) {
        Ok(connection) => connection,
        Err(response) => return response,
    };
    match runtime().and_then(|runtime| {
        runtime.block_on(run_statement(&connection, sql, abi::max_rows(request)))
    }) {
        Ok((columns, rows, truncated)) => abi::ok(Map::from_iter([
            ("connectionId".to_string(), Value::String(connection_id)),
            (
                "columns".to_string(),
                Value::Array(columns.into_iter().map(Value::String).collect()),
            ),
            (
                "rows".to_string(),
                Value::Array(rows.into_iter().map(Value::Array).collect()),
            ),
            ("truncated".to_string(), Value::Bool(truncated)),
        ])),
        Err(err) => abi::error("connector.queryFailed", connection.config.redact(&err)),
    }
}

fn metadata(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let connection = match connection(&connection_id) {
        Ok(connection) => connection,
        Err(response) => return response,
    };
    match runtime().and_then(|runtime| runtime.block_on(load_metadata(&connection))) {
        Ok(metadata) => abi::ok(Map::from_iter([
            ("connectionId".to_string(), Value::String(connection_id)),
            ("metadata".to_string(), metadata),
        ])),
        Err(err) => abi::error("connector.metadataFailed", connection.config.redact(&err)),
    }
}

fn close(request: &Value) -> IrodoriConnectorBuffer {
    let connection_id = abi::connection_id(Some(request));
    let mut guard = match connections().lock() {
        Ok(guard) => guard,
        Err(_) => {
            return abi::error(
                "connector.statePoisoned",
                "Connector connection state is poisoned.",
            )
        }
    };
    let existed = guard.remove(&connection_id).is_some();
    abi::ok(Map::from_iter([
        ("connectionId".to_string(), Value::String(connection_id)),
        ("closed".to_string(), Value::Bool(existed)),
    ]))
}

impl TrinoConnection {
    fn auth(&self, builder: RequestBuilder) -> RequestBuilder {
        let builder = builder
            .header("X-Trino-User", &self.config.user)
            .header("X-Presto-User", &self.config.user);
        let builder = if let Some(catalog) = self.config.catalog.as_deref() {
            builder
                .header("X-Trino-Catalog", catalog)
                .header("X-Presto-Catalog", catalog)
        } else {
            builder
        };
        let builder = if let Some(schema) = self.config.schema.as_deref() {
            builder
                .header("X-Trino-Schema", schema)
                .header("X-Presto-Schema", schema)
        } else {
            builder
        };
        if let Some(token) = self.config.bearer_token.as_deref() {
            builder.bearer_auth(token)
        } else if let Some(password) = self.config.password.as_deref() {
            builder.basic_auth(&self.config.user, Some(password))
        } else {
            builder
        }
    }
}

impl TrinoConfig {
    fn from_request(request: &Value) -> Result<Self, String> {
        let base_url = option_string(request, &["connectionString", "url", "dsn"])
            .unwrap_or_else(|| build_url(request));
        let user =
            option_string(request, &["user", "username"]).unwrap_or_else(|| "irodori".to_string());
        let password = option_string(request, &["password"]);
        let bearer_token = option_string(request, &["token", "bearerToken", "accessToken"]);
        let catalog = option_string(request, &["catalog"]);
        let schema = option_string(request, &["schema", "database", "db"]);
        let mut redaction_values = Vec::new();
        push_sensitive(&mut redaction_values, password.as_deref());
        push_sensitive(&mut redaction_values, bearer_token.as_deref());
        collect_url_auth(&base_url, &mut redaction_values);
        Ok(Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            user,
            password,
            bearer_token,
            catalog,
            schema,
            redaction_values,
        })
    }

    fn redact(&self, message: &str) -> String {
        self.redaction_values.iter().fold(
            message.replace(&self.base_url, "<trino-url>"),
            |message, secret| {
                if secret.is_empty() {
                    message
                } else {
                    message.replace(secret, "****")
                }
            },
        )
    }
}

async fn load_version(connection: &TrinoConnection) -> Result<String, String> {
    let response = connection
        .auth(
            connection
                .client
                .get(format!("{}/v1/info", connection.config.base_url)),
        )
        .send()
        .await
        .map_err(|err| format!("Trino info request failed: {err}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| format!("Trino info response read failed: {err}"))?;
    if !status.is_success() {
        return Err(format!("Trino info returned HTTP {status}: {text}"));
    }
    let value = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
    Ok(value
        .get("nodeVersion")
        .or_else(|| value.get("version"))
        .and_then(Value::as_str)
        .map(|version| format!("Trino/Presto {version}"))
        .unwrap_or_else(|| "Trino/Presto".to_string()))
}

async fn run_statement(
    connection: &TrinoConnection,
    sql: &str,
    cap: usize,
) -> Result<QueryOutput, String> {
    let mut response = connection
        .auth(
            connection
                .client
                .post(format!("{}/v1/statement", connection.config.base_url)),
        )
        .body(sql.to_string())
        .send()
        .await
        .map_err(|err| format!("Trino statement request failed: {err}"))?;
    let mut columns = Vec::new();
    let mut rows = Vec::new();
    let mut truncated = false;

    loop {
        let value = read_statement_response(response).await?;
        if columns.is_empty() {
            columns = value
                .get("columns")
                .and_then(Value::as_array)
                .map(|cols| {
                    cols.iter()
                        .map(|col| {
                            col.get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("value")
                                .to_string()
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
        }
        if let Some(data) = value.get("data").and_then(Value::as_array) {
            for row in data {
                if rows.len() >= cap {
                    truncated = true;
                    break;
                }
                rows.push(match row {
                    Value::Array(values) => values.clone(),
                    other => vec![other.clone()],
                });
            }
        }
        if truncated {
            break;
        }
        let Some(next_uri) = value.get("nextUri").and_then(Value::as_str) else {
            break;
        };
        response = connection
            .auth(connection.client.get(next_uri))
            .send()
            .await
            .map_err(|err| format!("Trino nextUri request failed: {err}"))?;
    }
    Ok((columns, rows, truncated))
}

async fn read_statement_response(response: reqwest::Response) -> Result<Value, String> {
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| format!("Trino statement response read failed: {err}"))?;
    if !status.is_success() {
        return Err(format!("Trino statement returned HTTP {status}: {text}"));
    }
    let value = serde_json::from_str::<Value>(&text)
        .map_err(|err| format!("Trino statement JSON failed: {err}: {text}"))?;
    if let Some(error) = value.get("error") {
        return Err(format!("Trino query failed: {error}"));
    }
    Ok(value)
}

async fn load_metadata(connection: &TrinoConnection) -> Result<Value, String> {
    let sql = "SELECT table_schema, table_name, column_name, data_type, ordinal_position \
               FROM information_schema.columns \
               WHERE table_schema NOT IN ('information_schema') \
               ORDER BY table_schema, table_name, ordinal_position";
    let (columns, rows, _) = run_statement(connection, sql, 10_000).await?;
    Ok(metadata_from_columns(&columns, rows))
}

fn metadata_from_columns(columns: &[String], rows: QueryRows) -> Value {
    let schema_idx = columns.iter().position(|column| column == "table_schema");
    let table_idx = columns.iter().position(|column| column == "table_name");
    let column_idx = columns.iter().position(|column| column == "column_name");
    let type_idx = columns.iter().position(|column| column == "data_type");
    let ordinal_idx = columns
        .iter()
        .position(|column| column == "ordinal_position");
    let mut schemas: BTreeMap<String, BTreeMap<String, ObjectMeta>> = BTreeMap::new();
    let (Some(schema_idx), Some(table_idx), Some(column_idx), Some(type_idx)) =
        (schema_idx, table_idx, column_idx, type_idx)
    else {
        return json!({ "schemas": [] });
    };
    for row in rows {
        let schema = string_cell(&row, schema_idx);
        let table = string_cell(&row, table_idx);
        let column = string_cell(&row, column_idx);
        if schema.is_empty() || table.is_empty() || column.is_empty() {
            continue;
        }
        let ordinal = ordinal_idx
            .and_then(|idx| row.get(idx))
            .and_then(Value::as_i64)
            .unwrap_or(0);
        let object = schemas
            .entry(schema.clone())
            .or_default()
            .entry(table.clone())
            .or_insert_with(|| ObjectMeta {
                schema,
                name: table,
                columns: Vec::new(),
            });
        object.columns.push(json!({
            "name": column,
            "dataType": string_cell(&row, type_idx),
            "nullable": true,
            "ordinal": ordinal
        }));
    }
    json!({
        "schemas": schemas
            .into_iter()
            .map(|(name, objects)| {
                json!({
                    "name": name,
                    "objects": objects
                        .into_values()
                        .map(|object| {
                            json!({
                                "schema": object.schema,
                                "name": object.name,
                                "kind": "table",
                                "columns": object.columns,
                                "indexes": [],
                                "primaryKey": [],
                                "foreignKeys": []
                            })
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect::<Vec<_>>()
    })
}

fn string_cell(row: &[Value], index: usize) -> String {
    row.get(index)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn build_url(request: &Value) -> String {
    let host = option_string(request, &["host", "endpoint"]).unwrap_or_else(|| "127.0.0.1".into());
    let port = option_string(request, &["port"]).unwrap_or_else(|| "8080".into());
    let scheme = if bool_option(request, &["tls", "ssl"]).unwrap_or(false) {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{host}:{port}")
}

fn connection(connection_id: &str) -> Result<TrinoConnection, IrodoriConnectorBuffer> {
    let guard = connections().lock().map_err(|_| {
        abi::error(
            "connector.statePoisoned",
            "Connector connection state is poisoned.",
        )
    })?;
    guard.get(connection_id).cloned().ok_or_else(|| {
        abi::error(
            "connector.connectionNotFound",
            format!("no open connection: {connection_id}"),
        )
    })
}

fn request_containers(request: &Value) -> Vec<&Value> {
    [
        Some(request),
        request.get("profile"),
        request.get("options"),
        request.get("auth"),
        request.get("secrets"),
        request
            .get("profile")
            .and_then(|profile| profile.get("options")),
        request
            .get("profile")
            .and_then(|profile| profile.get("auth")),
        request
            .get("profile")
            .and_then(|profile| profile.get("secrets")),
    ]
    .into_iter()
    .flatten()
    .collect()
}

fn option_string(request: &Value, fields: &[&str]) -> Option<String> {
    request_containers(request)
        .into_iter()
        .find_map(|container| {
            fields.iter().find_map(|field| {
                container
                    .get(*field)
                    .map(|value| match value {
                        Value::String(value) => value.clone(),
                        Value::Number(value) => value.to_string(),
                        Value::Bool(value) => value.to_string(),
                        _ => String::new(),
                    })
                    .map(|value| value.trim().to_string())
                    .filter(|value| !value.is_empty())
            })
        })
}

fn bool_option(request: &Value, fields: &[&str]) -> Option<bool> {
    request_containers(request)
        .into_iter()
        .find_map(|container| {
            fields
                .iter()
                .find_map(|field| container.get(*field).and_then(Value::as_bool))
        })
}

fn push_sensitive(values: &mut Vec<String>, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        if !values.iter().any(|existing| existing == value) {
            values.push(value.to_string());
        }
    }
}

fn collect_url_auth(url: &str, values: &mut Vec<String>) {
    let Some(after_scheme) = url.split_once("://").map(|(_, rest)| rest) else {
        return;
    };
    let Some(auth) = after_scheme
        .split('/')
        .next()
        .and_then(|host| host.split('@').next())
    else {
        return;
    };
    if auth.contains(':') {
        for part in auth.split(':') {
            push_sensitive(values, Some(part));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_config_from_profile() {
        let config = TrinoConfig::from_request(&json!({
            "profile": {
                "host": "trino.local",
                "port": 8443,
                "tls": true,
                "user": "alice",
                "catalog": "hive",
                "schema": "default"
            }
        }))
        .unwrap();
        assert_eq!(config.base_url, "https://trino.local:8443");
        assert_eq!(config.user, "alice");
        assert_eq!(config.catalog.as_deref(), Some("hive"));
    }

    #[test]
    fn builds_metadata_from_columns() {
        let columns = vec![
            "table_schema".to_string(),
            "table_name".to_string(),
            "column_name".to_string(),
            "data_type".to_string(),
            "ordinal_position".to_string(),
        ];
        let rows = vec![vec![
            json!("public"),
            json!("orders"),
            json!("id"),
            json!("bigint"),
            json!(1),
        ]];
        let metadata = metadata_from_columns(&columns, rows);
        assert_eq!(metadata["schemas"][0]["objects"][0]["name"], "orders");
    }
}
