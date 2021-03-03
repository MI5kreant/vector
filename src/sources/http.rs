use crate::{
    config::{log_schema, DataType, GlobalOptions, Resource, SourceConfig, SourceDescription},
    event::{Event, Value},
    shutdown::ShutdownSignal,
    sources::util::{add_query_parameters, ErrorMessage, HttpSource, HttpSourceAuthConfig},
    tls::TlsConfig,
    Pipeline,
};
use bytes::{Bytes, BytesMut};
use chrono::Utc;
use codec::BytesDelimitedCodec;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::{collections::HashMap, net::SocketAddr};

use tokio_util::codec::Decoder;
use warp::http::{HeaderMap, HeaderValue, StatusCode};

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(default)]
pub struct SimpleHttpConfig {
    address: SocketAddr,
    encoding: Encoding,
    headers: Vec<String>,
    query_parameters: Vec<String>,
    tls: Option<TlsConfig>,
    auth: Option<HttpSourceAuthConfig>,
    strict_path: bool,
    url_path: String,
    path_key: String,
}

inventory::submit! {
    SourceDescription::new::<SimpleHttpConfig>("http")
}

impl Default for SimpleHttpConfig {
    fn default() -> Self {
        Self {
            address: "0.0.0.0:80".parse().unwrap(),
            encoding: Default::default(),
            headers: Vec::new(),
            query_parameters: Vec::new(),
            tls: None,
            auth: None,
            path_key: "path".to_string(),
            url_path: "/".to_string(),
            strict_path: true,
        }
    }
}

impl_generate_config_from_default!(SimpleHttpConfig);

#[derive(Clone)]
struct SimpleHttpSource {
    encoding: Encoding,
    headers: Vec<String>,
    query_parameters: Vec<String>,
    path_key: String,
}

#[derive(Deserialize, Serialize, Debug, Eq, PartialEq, Clone, Derivative, Copy)]
#[serde(rename_all = "snake_case")]
#[derivative(Default)]
pub enum Encoding {
    #[derivative(Default)]
    Text,
    Ndjson,
    Json,
}

impl HttpSource for SimpleHttpSource {
    fn build_event(
        &self,
        body: Bytes,
        header_map: HeaderMap,
        query_parameters: HashMap<String, String>,
        request_path: &str,
    ) -> Result<Vec<Event>, ErrorMessage> {
        decode_body(body, self.encoding)
            .map(|events| add_headers(events, &self.headers, header_map))
            .map(|events| add_query_parameters(events, &self.query_parameters, query_parameters))
            .map(|events| add_path(events, self.path_key.as_str(), request_path))
            .map(|mut events| {
                // Add source type
                let key = log_schema().source_type_key();
                for event in events.iter_mut() {
                    event.as_mut_log().try_insert(key, Bytes::from("http"));
                }
                events
            })
    }
}

#[async_trait::async_trait]
#[typetag::serde(name = "http")]
impl SourceConfig for SimpleHttpConfig {
    async fn build(
        &self,
        _: &str,
        _: &GlobalOptions,
        shutdown: ShutdownSignal,
        out: Pipeline,
    ) -> crate::Result<super::Source> {
        let source = SimpleHttpSource {
            encoding: self.encoding,
            headers: self.headers.clone(),
            query_parameters: self.query_parameters.clone(),
            path_key: self.path_key.clone(),
        };
        source.run(
            self.address,
            &self.url_path.as_str(),
            self.strict_path,
            &self.tls,
            &self.auth,
            out,
            shutdown,
        )
    }

    fn output_type(&self) -> DataType {
        DataType::Log
    }

    fn source_type(&self) -> &'static str {
        "http"
    }

    fn resources(&self) -> Vec<Resource> {
        vec![Resource::tcp(self.address)]
    }
}

fn add_path(mut events: Vec<Event>, key: &str, path: &str) -> Vec<Event> {
    for event in events.iter_mut() {
        event
            .as_mut_log()
            .insert(key, Value::from(path.to_string()));
    }

    events
}

fn add_headers(
    mut events: Vec<Event>,
    headers_config: &[String],
    headers: HeaderMap,
) -> Vec<Event> {
    for header_name in headers_config {
        let value = headers.get(header_name).map(HeaderValue::as_bytes);

        for event in events.iter_mut() {
            event.as_mut_log().insert(
                header_name as &str,
                Value::from(value.map(Bytes::copy_from_slice)),
            );
        }
    }

    events
}

fn body_to_lines(buf: Bytes) -> impl Iterator<Item = Result<Bytes, ErrorMessage>> {
    let mut body = BytesMut::new();
    body.extend_from_slice(&buf);

    let mut decoder = BytesDelimitedCodec::new(b'\n');
    std::iter::from_fn(move || {
        match decoder.decode_eof(&mut body) {
            Err(error) => Some(Err(ErrorMessage::new(
                StatusCode::BAD_REQUEST,
                format!("Bad request: {}", error),
            ))),
            Ok(Some(b)) => Some(Ok(b)),
            Ok(None) => None, // actually done
        }
    })
    .filter(|s| match s {
        // filter empty lines
        Ok(b) => !b.is_empty(),
        _ => true,
    })
}

fn decode_body(body: Bytes, enc: Encoding) -> Result<Vec<Event>, ErrorMessage> {
    match enc {
        Encoding::Text => body_to_lines(body)
            .map(|r| Ok(Event::from(r?)))
            .collect::<Result<_, _>>(),
        Encoding::Ndjson => body_to_lines(body)
            .map(|j| {
                let parsed_json = serde_json::from_slice(&j?)
                    .map_err(|error| json_error(format!("Error parsing Ndjson: {:?}", error)))?;
                json_parse_object(parsed_json)
            })
            .collect::<Result<_, _>>(),
        Encoding::Json => {
            let parsed_json = serde_json::from_slice(&body)
                .map_err(|error| json_error(format!("Error parsing Json: {:?}", error)))?;
            json_parse_array_of_object(parsed_json)
        }
    }
}

fn json_parse_object(value: JsonValue) -> Result<Event, ErrorMessage> {
    let mut event = Event::new_empty_log();
    let log = event.as_mut_log();
    log.insert(log_schema().timestamp_key(), Utc::now()); // Add timestamp
    match value {
        JsonValue::Object(map) => {
            for (k, v) in map {
                log.insert_flat(k, v);
            }
            Ok(event)
        }
        _ => Err(json_error(format!(
            "Expected Object, got {}",
            json_value_to_type_string(&value)
        ))),
    }
}

fn json_parse_array_of_object(value: JsonValue) -> Result<Vec<Event>, ErrorMessage> {
    match value {
        JsonValue::Array(v) => v
            .into_iter()
            .map(json_parse_object)
            .collect::<Result<_, _>>(),
        JsonValue::Object(map) => {
            //treat like an array of one object
            Ok(vec![json_parse_object(JsonValue::Object(map))?])
        }
        _ => Err(json_error(format!(
            "Expected Array or Object, got {}.",
            json_value_to_type_string(&value)
        ))),
    }
}

fn json_error(s: String) -> ErrorMessage {
    ErrorMessage::new(StatusCode::BAD_REQUEST, format!("Bad JSON: {}", s))
}

fn json_value_to_type_string(value: &JsonValue) -> &'static str {
    match value {
        JsonValue::Object(_) => "Object",
        JsonValue::Array(_) => "Array",
        JsonValue::String(_) => "String",
        JsonValue::Number(_) => "Number",
        JsonValue::Bool(_) => "Bool",
        JsonValue::Null => "Null",
    }
}

#[cfg(test)]
mod tests {
    use super::{Encoding, SimpleHttpConfig};

    use crate::shutdown::ShutdownSignal;
    use crate::{
        config::{log_schema, GlobalOptions, SourceConfig},
        event::{Event, Value},
        test_util::{collect_n, next_addr, trace_init, wait_for_tcp},
        Pipeline,
    };
    use flate2::{
        write::{DeflateEncoder, GzEncoder},
        Compression,
    };
    use http::HeaderMap;
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;
    use std::io::Write;
    use std::net::SocketAddr;
    use tokio::sync::mpsc;

    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<SimpleHttpConfig>();
    }

    async fn source(
        encoding: Encoding,
        headers: Vec<String>,
        query_parameters: Vec<String>,
        path_key: &str,
        url_path: &str,
        strict_path: bool,
    ) -> (mpsc::Receiver<Event>, SocketAddr) {
        let (sender, recv) = Pipeline::new_test();
        let address = next_addr();
        let url_path = url_path.to_owned();
        let path_key = path_key.to_owned();
        tokio::spawn(async move {
            SimpleHttpConfig {
                address,
                encoding,
                headers,
                query_parameters,
                tls: None,
                auth: None,
                strict_path,
                path_key,
                url_path,
            }
            .build(
                "default",
                &GlobalOptions::default(),
                ShutdownSignal::noop(),
                sender,
            )
            .await
            .unwrap()
            .await
            .unwrap();
        });
        wait_for_tcp(address).await;
        (recv, address)
    }

    async fn send(address: SocketAddr, body: &str) -> u16 {
        reqwest::Client::new()
            .post(&format!("http://{}/", address))
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn send_with_headers(address: SocketAddr, body: &str, headers: HeaderMap) -> u16 {
        reqwest::Client::new()
            .post(&format!("http://{}/", address))
            .headers(headers)
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn send_with_query(address: SocketAddr, body: &str, query: &str) -> u16 {
        reqwest::Client::new()
            .post(&format!("http://{}?{}", address, query))
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn send_with_path(address: SocketAddr, body: &str, path: &str) -> u16 {
        reqwest::Client::new()
            .post(&format!("http://{}{}", address, path))
            .body(body.to_owned())
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    async fn send_bytes(address: SocketAddr, body: Vec<u8>, headers: HeaderMap) -> u16 {
        reqwest::Client::new()
            .post(&format!("http://{}/", address))
            .headers(headers)
            .body(body)
            .send()
            .await
            .unwrap()
            .status()
            .as_u16()
    }

    #[tokio::test]
    async fn http_multiline_text() {
        trace_init();

        let body = "test body\n\ntest body 2";

        let (rx, addr) = source(Encoding::default(), vec![], vec![], "http_path", "/", true).await;

        assert_eq!(200, send(addr, body).await);

        let mut events = collect_n(rx, 2).await;
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log[log_schema().message_key()], "test body".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
            assert_eq!(log["http_path"], "/".into());
        }
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log[log_schema().message_key()], "test body 2".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
            assert_eq!(log["http_path"], "/".into());
        }
    }

    #[tokio::test]
    async fn http_multiline_text2() {
        trace_init();

        //same as above test but with a newline at the end
        let body = "test body\n\ntest body 2\n";

        let (rx, addr) = source(Encoding::default(), vec![], vec![], "http_path", "/", true).await;

        assert_eq!(200, send(addr, body).await);

        let mut events = collect_n(rx, 2).await;
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log[log_schema().message_key()], "test body".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
            assert_eq!(log["http_path"], "/".into());
        }
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log[log_schema().message_key()], "test body 2".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
            assert_eq!(log["http_path"], "/".into());
        }
    }

    #[tokio::test]
    async fn http_json_parsing() {
        trace_init();

        let (rx, addr) = source(Encoding::Json, vec![], vec![], "http_path", "/", true).await;

        assert_eq!(400, send(addr, "{").await); //malformed
        assert_eq!(400, send(addr, r#"{"key"}"#).await); //key without value

        assert_eq!(200, send(addr, "{}").await); //can be one object or array of objects
        assert_eq!(200, send(addr, "[{},{},{}]").await);

        let mut events = collect_n(rx, 2).await;
        assert!(events
            .remove(1)
            .as_log()
            .get(log_schema().timestamp_key())
            .is_some());
        assert!(events
            .remove(0)
            .as_log()
            .get(log_schema().timestamp_key())
            .is_some());
    }

    #[tokio::test]
    async fn http_json_values() {
        trace_init();

        let (rx, addr) = source(Encoding::Json, vec![], vec![], "http_path", "/", true).await;

        assert_eq!(200, send(addr, r#"[{"key":"value"}]"#).await);
        assert_eq!(200, send(addr, r#"{"key2":"value2"}"#).await);

        let mut events = collect_n(rx, 2).await;
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log["key"], "value".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
            assert_eq!(log["http_path"], "/".into());
        }
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log["key2"], "value2".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
            assert_eq!(log["http_path"], "/".into());
        }
    }

    #[tokio::test]
    async fn http_json_dotted_keys() {
        trace_init();

        let (rx, addr) = source(Encoding::Json, vec![], vec![], "http_path", "/", true).await;

        assert_eq!(200, send(addr, r#"[{"dotted.key":"value"}]"#).await);
        assert_eq!(
            200,
            send(addr, r#"{"nested":{"dotted.key2":"value2"}}"#).await
        );

        let mut events = collect_n(rx, 2).await;
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log.get_flat("dotted.key").unwrap(), &Value::from("value"));
        }
        {
            let event = events.remove(0);
            let log = event.as_log();
            let mut map = BTreeMap::new();
            map.insert("dotted.key2".to_string(), Value::from("value2"));
            assert_eq!(log["nested"], map.into());
        }
    }

    #[tokio::test]
    async fn http_ndjson() {
        trace_init();

        let (rx, addr) = source(Encoding::Ndjson, vec![], vec![], "http_path", "/", true).await;

        assert_eq!(400, send(addr, r#"[{"key":"value"}]"#).await); //one object per line

        assert_eq!(
            200,
            send(addr, "{\"key1\":\"value1\"}\n\n{\"key2\":\"value2\"}").await
        );

        let mut events = collect_n(rx, 2).await;
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log["key1"], "value1".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
            assert_eq!(log["http_path"], "/".into());
        }
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log["key2"], "value2".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
            assert_eq!(log["http_path"], "/".into());
        }
    }

    #[tokio::test]
    async fn http_headers() {
        trace_init();

        let mut headers = HeaderMap::new();
        headers.insert("User-Agent", "test_client".parse().unwrap());
        headers.insert("Upgrade-Insecure-Requests", "false".parse().unwrap());

        let (rx, addr) = source(
            Encoding::Ndjson,
            vec![
                "User-Agent".to_string(),
                "Upgrade-Insecure-Requests".to_string(),
                "AbsentHeader".to_string(),
            ],
            vec![],
            "http_path",
            "/",
            true,
        )
        .await;

        assert_eq!(
            200,
            send_with_headers(addr, "{\"key1\":\"value1\"}", headers).await
        );

        let mut events = collect_n(rx, 1).await;
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log["key1"], "value1".into());
            assert_eq!(log["User-Agent"], "test_client".into());
            assert_eq!(log["Upgrade-Insecure-Requests"], "false".into());
            assert_eq!(log["AbsentHeader"], Value::Null);
            assert_eq!(log["http_path"], "/".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
        }
    }

    #[tokio::test]
    async fn http_query() {
        trace_init();
        let (rx, addr) = source(
            Encoding::Ndjson,
            vec![],
            vec![
                "source".to_string(),
                "region".to_string(),
                "absent".to_string(),
            ],
            "http_path",
            "/",
            true,
        )
        .await;

        assert_eq!(
            200,
            send_with_query(addr, "{\"key1\":\"value1\"}", "source=staging&region=gb").await
        );

        let mut events = collect_n(rx, 1).await;
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log["key1"], "value1".into());
            assert_eq!(log["source"], "staging".into());
            assert_eq!(log["region"], "gb".into());
            assert_eq!(log["absent"], Value::Null);
            assert_eq!(log["http_path"], "/".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
        }
    }

    #[tokio::test]
    async fn http_gzip_deflate() {
        trace_init();

        let body = "test body";

        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body.as_bytes()).unwrap();
        let body = encoder.finish().unwrap();

        let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(body.as_slice()).unwrap();
        let body = encoder.finish().unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("Content-Encoding", "gzip, deflate".parse().unwrap());

        let (rx, addr) = source(Encoding::default(), vec![], vec![], "http_path", "/", true).await;

        assert_eq!(200, send_bytes(addr, body, headers).await);

        let mut events = collect_n(rx, 1).await;
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log[log_schema().message_key()], "test body".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
            assert_eq!(log["http_path"], "/".into());
        }
    }

    #[tokio::test]
    async fn http_path() {
        trace_init();
        let (rx, addr) = source(
            Encoding::Ndjson,
            vec![],
            vec![],
            "vector_http_path",
            "/event/path",
            true,
        )
        .await;

        assert_eq!(
            200,
            send_with_path(addr, "{\"key1\":\"value1\"}", "/event/path").await
        );

        let mut events = collect_n(rx, 1).await;
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log["key1"], "value1".into());
            assert_eq!(log["vector_http_path"], "/event/path".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
        }
    }

    #[tokio::test]
    async fn http_path_no_restriction() {
        trace_init();
        let (rx, addr) = source(
            Encoding::Ndjson,
            vec![],
            vec![],
            "vector_http_path",
            "/event",
            false,
        )
        .await;

        assert_eq!(
            200,
            send_with_path(addr, "{\"key1\":\"value1\"}", "/event/path1").await
        );
        assert_eq!(
            200,
            send_with_path(addr, "{\"key2\":\"value2\"}", "/event/path2").await
        );

        let mut events = collect_n(rx, 2).await;
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log["key1"], "value1".into());
            assert_eq!(log["vector_http_path"], "/event/path1".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
        }
        {
            let event = events.remove(0);
            let log = event.as_log();
            assert_eq!(log["key2"], "value2".into());
            assert_eq!(log["vector_http_path"], "/event/path2".into());
            assert!(log.get(log_schema().timestamp_key()).is_some());
            assert_eq!(log[log_schema().source_type_key()], "http".into());
        }
    }

    #[tokio::test]
    async fn http_wrong_path() {
        trace_init();
        let (_rx, addr) = source(
            Encoding::Ndjson,
            vec![],
            vec![],
            "vector_http_path",
            "/",
            true,
        )
        .await;

        assert_eq!(
            405,
            send_with_path(addr, "{\"key1\":\"value1\"}", "/event/path").await
        );
    }
}
