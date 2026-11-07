//! Sidecar protocol v1 messages.

use serde::{Deserialize, Serialize};

use crate::catalog::{Catalog, PlaceBranch, TagBranch, TagDefaults};
use crate::error::SidecarError;

/// The operation a `request` asks the sidecar to run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Presence,
    PeopleCount,
    PlaceClass,
    PlaceTags,
    AtmosphereTags,
}

impl Op {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Presence => "presence",
            Self::PeopleCount => "people_count",
            Self::PlaceClass => "place_class",
            Self::PlaceTags => "place_tags",
            Self::AtmosphereTags => "atmosphere_tags",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "presence" => Some(Self::Presence),
            "people_count" => Some(Self::PeopleCount),
            "place_class" => Some(Self::PlaceClass),
            "place_tags" => Some(Self::PlaceTags),
            "atmosphere_tags" => Some(Self::AtmosphereTags),
            _ => None,
        }
    }
}

/// A temporary 1024px JPEG the sidecar is allowed to read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    pub path: String,
    pub mtime_ms: u64,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    pub quantization: String,
    pub revision: String,
    pub catalog_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    pub kind: String,
    pub device: String,
}

/// The sidecar's `hello`, printed after loading the model and mmproj.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol_version: u32,
    pub sidecar_version: String,
    pub plugin_id: String,
    pub variant: String,
    pub model: ModelInfo,
    pub backend: BackendInfo,
    pub capabilities: Vec<String>,
    pub defaults: TagDefaults,
    pub catalog: Catalog,
}

impl Hello {
    pub fn supports_atmosphere(&self) -> bool {
        self.capabilities.iter().any(|c| c == "atmosphere_tags")
    }
}

/// The sidecar's `--probe` output, printed without loading the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Probe {
    pub protocol_version: u32,
    pub sidecar_version: String,
    pub plugin_id: String,
    pub variant: String,
    pub catalog_version: String,
    pub backends: Vec<BackendInfo>,
}

/// A message sent from the host to the sidecar.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarRequest {
    Request {
        id: String,
        op: Op,
        image: ImageRef,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        branch: Option<TagBranch>,
    },
    Cancel {
        id: String,
    },
    Shutdown,
}

/// A message sent from the sidecar to the host.
///
/// `Hello` carries the full catalog and is the largest variant by design.
/// Messages are parsed one line at a time, so boxing it would add allocation
/// and match noise without a meaningful runtime benefit.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SidecarMessage {
    Hello(Hello),
    Probe(Probe),
    Response(SidecarResponse),
    Cancelled { id: String },
    Fatal { error: SidecarError },
    ShutdownAck,
}

/// The envelope of one request result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SidecarResponse {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<SidecarResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<SidecarError>,
}

/// The payload of a successful response, tagged by `op`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum SidecarResult {
    Presence {
        present: bool,
        p_yes: f32,
    },
    PeopleCount {
        count: u32,
        interval: [u32; 2],
    },
    PlaceClass {
        branch: PlaceBranch,
        probabilities: PlaceProbabilities,
    },
    PlaceTags {
        branch: TagBranch,
        tags: Vec<TagProbability>,
    },
    AtmosphereTags {
        tags: Vec<TagProbability>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PlaceProbabilities {
    pub indoor: f32,
    pub outdoor: f32,
    pub other: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagProbability {
    pub key: String,
    pub p_yes: f32,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::{PlaceBranch, TagBranch};
    use crate::error::SidecarErrorCode;

    fn round_trip_message(json: &str) -> SidecarMessage {
        let message: SidecarMessage = serde_json::from_str(json).unwrap();
        let value = serde_json::to_value(&message).unwrap();
        let again: SidecarMessage = serde_json::from_value(value).unwrap();
        assert_eq!(again, message);
        message
    }

    fn round_trip_request(json: &str) -> SidecarRequest {
        let request: SidecarRequest = serde_json::from_str(json).unwrap();
        let value = serde_json::to_value(&request).unwrap();
        let again: SidecarRequest = serde_json::from_value(value).unwrap();
        assert_eq!(again, request);
        request
    }

    #[test]
    fn ops_use_the_wire_strings() {
        let cases = [
            (Op::Presence, "presence"),
            (Op::PeopleCount, "people_count"),
            (Op::PlaceClass, "place_class"),
            (Op::PlaceTags, "place_tags"),
            (Op::AtmosphereTags, "atmosphere_tags"),
        ];
        for (op, text) in cases {
            assert_eq!(op.as_str(), text);
            assert_eq!(Op::parse(text), Some(op));
            assert_eq!(serde_json::to_value(op).unwrap(), text);
        }
        assert_eq!(Op::parse("unknown"), None);
    }

    #[test]
    fn hello_round_trips_and_keeps_the_catalog() {
        let json = r#"{"type":"hello","protocol_version":1,"sidecar_version":"1.0.0","plugin_id":"vrc-photon-autotag","variant":"linux-vulkan","model":{"id":"d1-2B","quantization":"Q8_0","revision":"rev","catalog_version":"2026-11-03.1"},"backend":{"kind":"vulkan","device":"Vulkan0"},"capabilities":["presence"],"defaults":{"presence_threshold":0.9,"multiple_threshold":0.5},"catalog":{"version":"2026-11-03.1","outdoor":[{"key":"sea","name":"海","name_en":"Sea","description":"海","default_threshold":0.5}],"indoor":[]}}"#;
        let SidecarMessage::Hello(hello) = serde_json::from_str(json).unwrap() else {
            panic!("expected hello");
        };
        assert_eq!(hello.protocol_version, 1);
        assert_eq!(hello.catalog.outdoor[0].key, "sea");
        assert_eq!(hello.defaults.presence_threshold, 0.9);

        let SidecarMessage::Hello(again) = round_trip_message(json) else {
            panic!("expected hello");
        };
        assert_eq!(again, hello);
        assert_eq!(again.catalog.outdoor[0].name, "海");
        assert_eq!(again.catalog.outdoor[0].name_en.as_deref(), Some("Sea"));
        assert!(again.catalog.indoor.is_empty());
    }

    #[test]
    fn catalog_name_en_is_optional() {
        let json = r#"{"type":"hello","protocol_version":1,"sidecar_version":"1.0.0","plugin_id":"vrc-photon-autotag","variant":"linux-vulkan","model":{"id":"d1-2B","quantization":"Q8_0","revision":"rev","catalog_version":"2026-11-03.1"},"backend":{"kind":"vulkan","device":"Vulkan0"},"capabilities":["presence"],"defaults":{"presence_threshold":0.9,"multiple_threshold":0.5},"catalog":{"version":"2026-11-03.1","outdoor":[],"indoor":[{"key":"japanese_room","name":"和室","description":"畳・障子のある和室","default_threshold":0.5}]}}"#;
        let SidecarMessage::Hello(hello) = serde_json::from_str(json).unwrap() else {
            panic!("expected hello");
        };
        assert_eq!(hello.catalog.indoor[0].key, "japanese_room");
        assert_eq!(hello.catalog.indoor[0].name_en, None);
    }

    #[test]
    fn probe_round_trips_the_advertised_backends() {
        let json = r#"{"type":"probe","protocol_version":1,"sidecar_version":"1.0.0","plugin_id":"vrc-photon-autotag","variant":"linux-vulkan","catalog_version":"2026-11-03.1","backends":[{"kind":"vulkan","device":"AMD Radeon ..."},{"kind":"cpu","device":"CPU"}]}"#;
        let SidecarMessage::Probe(probe) = round_trip_message(json) else {
            panic!("expected probe");
        };
        assert_eq!(probe.protocol_version, 1);
        assert_eq!(probe.catalog_version, "2026-11-03.1");
        assert_eq!(probe.backends.len(), 2);
        assert_eq!(probe.backends[0].kind, "vulkan");
        assert_eq!(probe.backends[1].device, "CPU");
    }

    #[test]
    fn request_response_round_trip() {
        let image = r#"{"path":"/home/user/.local/share/vrc-photon/cache/tagging/job.jpg","mtime_ms":1760000000000,"bytes":123456}"#;

        let presence = format!(r#"{{"type":"request","id":"r1","op":"presence","image":{image}}}"#);
        let SidecarRequest::Request {
            id,
            op,
            image: parsed_image,
            branch,
        } = round_trip_request(&presence)
        else {
            panic!("expected a request");
        };
        assert_eq!(id, "r1");
        assert_eq!(op, Op::Presence);
        assert_eq!(
            parsed_image.path,
            "/home/user/.local/share/vrc-photon/cache/tagging/job.jpg"
        );
        assert_eq!(parsed_image.mtime_ms, 1_760_000_000_000);
        assert_eq!(parsed_image.bytes, 123_456);
        assert_eq!(branch, None);

        let people_count =
            format!(r#"{{"type":"request","id":"r2","op":"people_count","image":{image}}}"#);
        let SidecarRequest::Request { id, op, branch, .. } = round_trip_request(&people_count)
        else {
            panic!("expected a request");
        };
        assert_eq!(id, "r2");
        assert_eq!(op, Op::PeopleCount);
        assert_eq!(branch, None);

        let place_class =
            format!(r#"{{"type":"request","id":"r3","op":"place_class","image":{image}}}"#);
        let SidecarRequest::Request { id, op, branch, .. } = round_trip_request(&place_class)
        else {
            panic!("expected a request");
        };
        assert_eq!(id, "r3");
        assert_eq!(op, Op::PlaceClass);
        assert_eq!(branch, None);

        let place_tags = format!(
            r#"{{"type":"request","id":"r4","op":"place_tags","branch":"outdoor","image":{image}}}"#
        );
        let SidecarRequest::Request { id, op, branch, .. } = round_trip_request(&place_tags) else {
            panic!("expected a request");
        };
        assert_eq!(id, "r4");
        assert_eq!(op, Op::PlaceTags);
        assert_eq!(branch, Some(TagBranch::Outdoor));

        let value = serde_json::to_value(SidecarRequest::Request {
            id: "r1".to_string(),
            op: Op::Presence,
            image: parsed_image,
            branch: None,
        })
        .unwrap();
        assert_eq!(value["type"], "request");
        assert_eq!(value["op"], "presence");
        assert!(value.get("branch").is_none());

        let presence_response = r#"{"type":"response","id":"r1","ok":true,"result":{"op":"presence","present":true,"p_yes":0.9971}}"#;
        let SidecarMessage::Response(response) = round_trip_message(presence_response) else {
            panic!("expected a response");
        };
        assert!(response.ok);
        assert_eq!(response.id, "r1");
        assert!(response.error.is_none());
        assert_eq!(
            response.result,
            Some(SidecarResult::Presence {
                present: true,
                p_yes: 0.9971,
            })
        );

        let people_response = r#"{"type":"response","id":"r2","ok":true,"result":{"op":"people_count","count":3,"interval":[2,4]}}"#;
        let SidecarMessage::Response(response) = round_trip_message(people_response) else {
            panic!("expected a response");
        };
        assert_eq!(
            response.result,
            Some(SidecarResult::PeopleCount {
                count: 3,
                interval: [2, 4],
            })
        );

        let place_class_response = r#"{"type":"response","id":"r3","ok":true,"result":{"op":"place_class","branch":"outdoor","probabilities":{"indoor":0.01,"outdoor":0.98,"other":0.01}}}"#;
        let SidecarMessage::Response(response) = round_trip_message(place_class_response) else {
            panic!("expected a response");
        };
        assert_eq!(
            response.result,
            Some(SidecarResult::PlaceClass {
                branch: PlaceBranch::Outdoor,
                probabilities: PlaceProbabilities {
                    indoor: 0.01,
                    outdoor: 0.98,
                    other: 0.01,
                },
            })
        );

        let place_tags_response = r#"{"type":"response","id":"r4","ok":true,"result":{"op":"place_tags","branch":"outdoor","tags":[{"key":"sea","p_yes":0.969},{"key":"night_view","p_yes":0.804}]}}"#;
        let SidecarMessage::Response(response) = round_trip_message(place_tags_response) else {
            panic!("expected a response");
        };
        assert_eq!(
            response.result,
            Some(SidecarResult::PlaceTags {
                branch: TagBranch::Outdoor,
                tags: vec![
                    TagProbability {
                        key: "sea".to_string(),
                        p_yes: 0.969,
                    },
                    TagProbability {
                        key: "night_view".to_string(),
                        p_yes: 0.804,
                    },
                ],
            })
        );
    }

    #[test]
    fn atmosphere_op_and_result_round_trip() {
        let request = round_trip_request(
            r#"{"type":"request","id":"r5","op":"atmosphere_tags","image":{"path":"/tmp/a.jpg","mtime_ms":1,"bytes":2}}"#,
        );
        let SidecarRequest::Request { op, branch, .. } = request else {
            panic!("expected request");
        };
        assert_eq!(op, Op::AtmosphereTags);
        assert_eq!(branch, None);

        let result: SidecarResult = serde_json::from_str(
            r#"{"op":"atmosphere_tags","tags":[{"key":"weather_clear","p_yes":0.9}]}"#,
        )
        .unwrap();
        assert!(
            matches!(result, SidecarResult::AtmosphereTags { tags } if tags[0].key == "weather_clear")
        );
    }

    #[test]
    fn hello_reports_atmosphere_capability() {
        let json = r#"{"type":"hello","protocol_version":1,"sidecar_version":"0.4.0","plugin_id":"vrc-photon-autotag","variant":"linux-vulkan","model":{"id":"d1-2B","quantization":"Q8_0","revision":"rev","catalog_version":"2026-11-07.1"},"backend":{"kind":"vulkan","device":"Vulkan0"},"capabilities":["presence","atmosphere_tags"],"defaults":{"presence_threshold":0.9,"multiple_threshold":0.5},"catalog":{"version":"2026-11-07.1","outdoor":[],"indoor":[],"atmosphere":[{"key":"weather_clear","name":"晴れ","description":"晴れ","default_threshold":0.5,"group":"weather"}]}}"#;
        let SidecarMessage::Hello(hello) = serde_json::from_str(json).unwrap() else {
            panic!("expected hello");
        };
        assert!(hello.supports_atmosphere());
    }

    #[test]
    fn error_response_round_trip() {
        let json = r#"{"type":"response","id":"r1","ok":false,"error":{"code":"image_decode_failed","message":"...","retryable":false}}"#;
        let SidecarMessage::Response(response) = round_trip_message(json) else {
            panic!("expected a response");
        };
        assert!(!response.ok);
        assert_eq!(response.id, "r1");
        assert!(response.result.is_none());
        let error = response.error.as_ref().unwrap();
        assert_eq!(error.code, SidecarErrorCode::ImageDecodeFailed);
        assert_eq!(error.message, "...");
        assert!(!error.retryable);

        let value = serde_json::to_value(SidecarMessage::Response(response)).unwrap();
        assert_eq!(value["type"], "response");
        assert!(value.get("result").is_none());
        assert_eq!(value["error"]["code"], "image_decode_failed");
    }

    #[test]
    fn fatal_before_hello_round_trips() {
        let json = r#"{"type":"fatal","error":{"code":"model_load_failed","message":"mmproj is missing","retryable":false}}"#;
        let SidecarMessage::Fatal { error } = round_trip_message(json) else {
            panic!("expected a fatal message");
        };
        assert_eq!(error.code, SidecarErrorCode::ModelLoadFailed);
        assert_eq!(error.message, "mmproj is missing");
        assert!(!error.retryable);

        let value = serde_json::to_value(SidecarMessage::Fatal { error }).unwrap();
        assert_eq!(value["type"], "fatal");
        assert_eq!(value["error"]["code"], "model_load_failed");
    }

    #[test]
    fn cancelled_and_shutdown_round_trip() {
        assert_eq!(
            round_trip_message(r#"{"type":"cancelled","id":"r1"}"#),
            SidecarMessage::Cancelled {
                id: "r1".to_string()
            }
        );
        assert_eq!(
            round_trip_message(r#"{"type":"shutdown_ack"}"#),
            SidecarMessage::ShutdownAck
        );

        assert_eq!(
            round_trip_request(r#"{"type":"cancel","id":"r1"}"#),
            SidecarRequest::Cancel {
                id: "r1".to_string()
            }
        );
        assert_eq!(
            round_trip_request(r#"{"type":"shutdown"}"#),
            SidecarRequest::Shutdown
        );
    }

    #[test]
    fn unknown_message_types_are_rejected() {
        assert!(serde_json::from_str::<SidecarMessage>(r#"{"type":"future"}"#).is_err());
        assert!(serde_json::from_str::<SidecarRequest>(r#"{"type":"future"}"#).is_err());
    }
}
