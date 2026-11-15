//! Built-in copies of the well-known-type files, served when a loader does
//! not have them. They carry only the definitions (no options), which is all
//! code generation needs.

/// The built-in source for an import path, if there is one.
pub(super) fn source(path: &str) -> Option<&'static str> {
    Some(match path {
        "google/protobuf/any.proto" => {
            "syntax = \"proto3\";\npackage google.protobuf;\n\
             // `Any` holds an arbitrary message with a URL naming its type.\n\
             message Any {\n  string type_url = 1;\n  bytes value = 2;\n}\n"
        }
        "google/protobuf/duration.proto" => {
            "syntax = \"proto3\";\npackage google.protobuf;\n\
             // A signed span of time.\n\
             message Duration {\n  int64 seconds = 1;\n  int32 nanos = 2;\n}\n"
        }
        "google/protobuf/timestamp.proto" => {
            "syntax = \"proto3\";\npackage google.protobuf;\n\
             // A point in time, as seconds and nanoseconds since the Unix epoch.\n\
             message Timestamp {\n  int64 seconds = 1;\n  int32 nanos = 2;\n}\n"
        }
        "google/protobuf/empty.proto" => {
            "syntax = \"proto3\";\npackage google.protobuf;\n\
             // A message with no fields.\n\
             message Empty {}\n"
        }
        "google/protobuf/field_mask.proto" => {
            "syntax = \"proto3\";\npackage google.protobuf;\n\
             // A set of field paths.\n\
             message FieldMask {\n  repeated string paths = 1;\n}\n"
        }
        "google/protobuf/wrappers.proto" => {
            "syntax = \"proto3\";\npackage google.protobuf;\n\
             // Wrapper for `double`.\nmessage DoubleValue { double value = 1; }\n\
             // Wrapper for `float`.\nmessage FloatValue { float value = 1; }\n\
             // Wrapper for `int64`.\nmessage Int64Value { int64 value = 1; }\n\
             // Wrapper for `uint64`.\nmessage UInt64Value { uint64 value = 1; }\n\
             // Wrapper for `int32`.\nmessage Int32Value { int32 value = 1; }\n\
             // Wrapper for `uint32`.\nmessage UInt32Value { uint32 value = 1; }\n\
             // Wrapper for `bool`.\nmessage BoolValue { bool value = 1; }\n\
             // Wrapper for `string`.\nmessage StringValue { string value = 1; }\n\
             // Wrapper for `bytes`.\nmessage BytesValue { bytes value = 1; }\n"
        }
        "google/protobuf/struct.proto" => {
            "syntax = \"proto3\";\npackage google.protobuf;\n\
             // A JSON object.\n\
             message Struct {\n  map<string, Value> fields = 1;\n}\n\
             // A JSON value.\n\
             message Value {\n  oneof kind {\n    NullValue null_value = 1;\n    double number_value = 2;\n    \
             string string_value = 3;\n    bool bool_value = 4;\n    Struct struct_value = 5;\n    \
             ListValue list_value = 6;\n  }\n}\n\
             // The JSON `null`.\n\
             enum NullValue {\n  NULL_VALUE = 0;\n}\n\
             // A JSON array.\n\
             message ListValue {\n  repeated Value values = 1;\n}\n"
        }
        _ => return None,
    })
}
