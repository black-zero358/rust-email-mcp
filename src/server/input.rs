//! Read-only argument contracts. Write inputs retain the legacy validator.
use crate::{
    accounts::{ACCOUNT_ID_MAX_CHARACTERS, valid_account_id},
    error::{InputViolation, MAX_INPUT_VIOLATIONS, MailError, Result},
    input_limits::ReadInputLimits,
};
use serde_json::{Value, json};

/// Standard schema lengths count code points; the extension publishes the
/// independent UTF-8 budget consumed by this validator.
pub(super) fn read_properties(name: &str, mut properties: Value) -> Value {
    if super::is_account_read(name) {
        properties["account_id"] = json!({"type":"string","minLength":1,"maxLength":ACCOUNT_ID_MAX_CHARACTERS,"x-maxUtf8Bytes":ACCOUNT_ID_MAX_CHARACTERS,"pattern":format!("^[A-Za-z0-9][A-Za-z0-9._-]{{0,{}}}$",ACCOUNT_ID_MAX_CHARACTERS-1),"description":"Omit to select the configured default read account; list_accounts shows accounts available to this principal."});
    }
    if properties.get("body_mode").is_some() {
        properties["html_display"] =
            json!({"type":"string","enum":["legacy","readable"],"default":"legacy"});
    }
    for (name, schema) in properties.as_object_mut().expect("static properties") {
        match name.as_str() {
            "mailbox" => {
                schema["maxLength"] = json!(ReadInputLimits::MAILBOX_CHARACTERS);
                schema["x-maxUtf8Bytes"] = json!(ReadInputLimits::TEXT_UTF8_BYTES);
                schema["x-maxModifiedUtf7Bytes"] = json!(ReadInputLimits::MAILBOX_WIRE_BYTES);
            }
            "mailboxes" => {
                schema["items"]["maxLength"] = json!(ReadInputLimits::MAILBOX_CHARACTERS);
                schema["items"]["x-maxUtf8Bytes"] = json!(ReadInputLimits::TEXT_UTF8_BYTES);
                schema["items"]["x-maxModifiedUtf7Bytes"] =
                    json!(ReadInputLimits::MAILBOX_WIRE_BYTES);
            }
            "query" | "from" | "to" | "subject" => {
                schema["maxLength"] = json!(ReadInputLimits::QUERY_CHARACTERS);
                schema["x-maxUtf8Bytes"] = json!(ReadInputLimits::TEXT_UTF8_BYTES);
            }
            // This lookup must accept exactly the keys used by legacy writes.
            "operation_key" => schema["x-maxUtf8Bytes"] = json!(128),
            "message_id" => schema["x-maxUtf8Bytes"] = json!(998),
            "since" | "before" | "date_start" | "date_end" => {
                schema["minLength"] = json!(10);
                schema["maxLength"] = json!(10);
                schema["x-maxUtf8Bytes"] = json!(10);
            }
            _ => {}
        }
    }
    properties
}

pub(super) fn validate(name: &str, args: &Value, schema: &Value) -> Result<()> {
    let mut violations = Vec::new();
    structural(args, schema, "", &mut violations);
    if violations.is_empty() {
        semantic(name, args, &mut violations);
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(MailError::input_fields(violations))
    }
}

fn push(out: &mut Vec<InputViolation>, issue: InputViolation) {
    if out.len() < MAX_INPUT_VIOLATIONS {
        out.push(issue);
    }
}

fn value_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_u64() || n.is_i64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_error(field: &str, expected: &'static str, actual: &Value, out: &mut Vec<InputViolation>) {
    let mut issue = InputViolation::new(field, "type_mismatch", "use_expected_type");
    issue.expected = Some(expected);
    issue.actual_type = Some(value_type(actual));
    push(out, issue);
}

fn pointer(parent: &str, key: &str) -> String {
    format!("{parent}/{}", key.replace('~', "~0").replace('/', "~1"))
}

fn structural(value: &Value, schema: &Value, field: &str, out: &mut Vec<InputViolation>) {
    if out.len() == MAX_INPUT_VIOLATIONS {
        return;
    }
    match schema["type"].as_str() {
        Some("object") => {
            let Some(object) = value.as_object() else {
                return type_error(field, "object", value, out);
            };
            let properties = schema["properties"].as_object().expect("static properties");
            if let Some(required) = schema["required"].as_array() {
                for key in required.iter().filter_map(Value::as_str) {
                    if !object.contains_key(key) {
                        push(
                            out,
                            InputViolation::new(
                                &pointer(field, key),
                                "required_field",
                                "provide_field",
                            ),
                        );
                    }
                }
            }
            for (key, value) in object {
                if out.len() == MAX_INPUT_VIOLATIONS {
                    break;
                }
                if let Some(schema) = properties.get(key) {
                    structural(value, schema, &pointer(field, key), out);
                } else {
                    // Report the containing object: arbitrary unknown key names
                    // may contain private text and are never echoed.
                    push(
                        out,
                        InputViolation::new(field, "unknown_field", "remove_unknown_fields"),
                    );
                }
            }
        }
        Some("string") => {
            let Some(text) = value.as_str() else {
                return type_error(field, "string", value, out);
            };
            let actual = text.chars().count();
            let min = schema["minLength"].as_u64().unwrap_or(0) as usize;
            let max = schema["maxLength"].as_u64().unwrap_or(65536) as usize;
            if actual < min {
                push(
                    out,
                    InputViolation::new(field, "character_minimum", "provide_longer_field")
                        .with_length(min, actual, "unicode_code_points"),
                );
            } else if actual > max {
                push(
                    out,
                    InputViolation::new(field, "character_limit", "shorten_field").with_length(
                        max,
                        actual,
                        "unicode_code_points",
                    ),
                );
            }
            if let Some(limit) = schema["x-maxUtf8Bytes"].as_u64().map(|n| n as usize)
                && text.len() > limit
            {
                push(
                    out,
                    InputViolation::new(field, "utf8_byte_limit", "shorten_field").with_length(
                        limit,
                        text.len(),
                        "utf8_bytes",
                    ),
                );
            }
            if text.contains(['\0', '\r']) {
                push(
                    out,
                    InputViolation::new(
                        field,
                        "forbidden_control_character",
                        "remove_control_characters",
                    ),
                );
            }
        }
        Some("integer") => {
            if !value.is_i64() && !value.is_u64() {
                return type_error(field, "integer", value, out);
            }
            let minimum = schema["minimum"].as_u64().unwrap_or(0);
            let maximum = schema["maximum"].as_u64().unwrap_or(u64::MAX);
            if value.as_u64().is_none_or(|n| n < minimum || n > maximum) {
                let mut issue = InputViolation::new(field, "integer_range", "use_expected_range");
                issue.minimum = Some(minimum);
                issue.maximum = Some(maximum);
                push(out, issue);
            }
        }
        Some("boolean") if !value.is_boolean() => type_error(field, "boolean", value, out),
        Some("array") => {
            let Some(array) = value.as_array() else {
                return type_error(field, "array", value, out);
            };
            let minimum = schema["minItems"].as_u64().unwrap_or(0) as usize;
            let maximum = schema["maxItems"].as_u64().unwrap_or(100) as usize;
            if array.len() < minimum || array.len() > maximum {
                let mut issue = InputViolation::new(field, "item_count", "use_expected_item_count")
                    .with_length(
                        if array.len() < minimum {
                            minimum
                        } else {
                            maximum
                        },
                        array.len(),
                        "items",
                    );
                issue.minimum = Some(minimum as u64);
                issue.maximum = Some(maximum as u64);
                push(out, issue);
            }
            for (i, value) in array.iter().enumerate() {
                if out.len() == MAX_INPUT_VIOLATIONS {
                    break;
                }
                structural(
                    value,
                    &schema["items"],
                    &pointer(field, &i.to_string()),
                    out,
                );
            }
        }
        _ => {}
    }
    if schema
        .get("const")
        .is_some_and(|expected| expected != value)
    {
        push(
            out,
            InputViolation::new(field, "constant_mismatch", "use_required_value"),
        );
    }
    if schema["enum"]
        .as_array()
        .is_some_and(|values| !values.contains(value))
    {
        let mut issue = InputViolation::new(field, "unsupported_value", "use_supported_value");
        issue.expected = Some("declared_enum_value");
        push(out, issue);
    }
}

fn semantic(name: &str, args: &Value, out: &mut Vec<InputViolation>) {
    if name == "read_conversation" {
        let conflicting = |field: &str, related: &'static str, out: &mut Vec<InputViolation>| {
            let mut issue = InputViolation::new(
                &pointer("", field),
                "conflicting_field",
                "remove_conflicting_field",
            );
            issue.related_field = Some(related);
            push(out, issue);
        };
        if args.get("cursor").is_some() {
            for field in [
                "mailbox",
                "uid",
                "uid_validity",
                "before_uid",
                "candidate_limit",
                "limit",
                "body_mode",
                "html_display",
                "max_body_length",
                "mailboxes",
                "scope",
            ] {
                if args.get(field).is_some() {
                    conflicting(field, "/cursor", out);
                }
            }
        } else {
            for field in ["uid", "uid_validity"] {
                if args.get(field).is_none() {
                    push(
                        out,
                        InputViolation::new(&pointer("", field), "required_field", "provide_field"),
                    );
                }
            }
            if args.get("scope").is_some() && args.get("mailboxes").is_some() {
                conflicting("mailboxes", "/scope", out);
            }
            if args.get("before_uid").is_some() {
                if args.get("scope").is_some() {
                    conflicting("before_uid", "/scope", out);
                } else if args.get("mailboxes").is_some() {
                    conflicting("before_uid", "/mailboxes", out);
                }
            }
        }
    }
    if super::is_account_read(name)
        && args["account_id"]
            .as_str()
            .is_some_and(|id| !valid_account_id(id))
    {
        push(
            out,
            InputViolation::new("/account_id", "invalid_account_id", "list_accounts"),
        );
    }
    if matches!(name, "download_attachment" | "prepare_attachment_download") {
        if args.get("attachment_id").is_none() && args.get("attachment_index").is_none() {
            push(
                out,
                InputViolation::new(
                    "",
                    "attachment_reference_required",
                    "provide_attachment_reference",
                ),
            );
        }
        if let Some(id) = args["attachment_id"].as_str()
            && !id.strip_prefix("att_v1_").is_some_and(|value| {
                value.len() == 64
                    && value
                        .bytes()
                        .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            })
        {
            push(
                out,
                InputViolation::new(
                    "/attachment_id",
                    "invalid_attachment_id",
                    "relist_attachments",
                ),
            );
        }
    }
    for key in ["mailbox", "query", "from", "to", "subject"] {
        if args[key].as_str().is_some_and(|text| text.contains('\n')) {
            push(
                out,
                InputViolation::new(
                    &pointer("", key),
                    "forbidden_control_character",
                    "remove_control_characters",
                ),
            );
        }
    }
    if let Some(mailboxes) = args["mailboxes"].as_array() {
        for (i, mailbox) in mailboxes.iter().enumerate() {
            let name = mailbox.as_str().expect("validated mailbox");
            let field = pointer("/mailboxes", &i.to_string());
            if name.is_empty() {
                push(
                    out,
                    InputViolation::new(&field, "character_minimum", "provide_longer_field")
                        .with_length(1, 0, "unicode_code_points"),
                );
            } else if name.chars().any(char::is_control) {
                push(
                    out,
                    InputViolation::new(
                        &field,
                        "forbidden_control_character",
                        "remove_control_characters",
                    ),
                );
            }
        }
    }
    for key in ["since", "before", "date_start", "date_end"] {
        if args[key]
            .as_str()
            .is_some_and(|text| !valid_date(text, if key.starts_with("date_") { 1970 } else { 1 }))
        {
            let mut issue =
                InputViolation::new(&pointer("", key), "invalid_date", "use_valid_calendar_date");
            issue.expected = Some("YYYY-MM-DD");
            push(out, issue);
        }
    }
    for (lower, upper) in [("since", "before"), ("date_start", "date_end")] {
        if let (Some(a), Some(b)) = (args[lower].as_str(), args[upper].as_str())
            && a >= b
        {
            let mut issue =
                InputViolation::new(&pointer("", upper), "conflicting_range", "adjust_range");
            issue.related_field = Some(if lower == "since" {
                "/since"
            } else {
                "/date_start"
            });
            push(out, issue);
        }
    }
    if name == "get_mailbox_statistics"
        && (args.get("date_start").is_some() || args.get("date_end").is_some())
    {
        for (key, related) in [("date_start", "/date_end"), ("date_end", "/date_start")] {
            if args.get(key).is_none() {
                let mut issue = InputViolation::new(
                    &pointer("", key),
                    "required_with_field",
                    "provide_paired_field",
                );
                issue.related_field = Some(related);
                push(out, issue);
            }
        }
        if let (Some(a), Some(b)) = (args["date_start"].as_str(), args["date_end"].as_str())
            && valid_date(a, 1970)
            && valid_date(b, 1970)
            && a < b
        {
            let days = date_ordinal(b) - date_ordinal(a);
            if days > 31 {
                push(
                    out,
                    InputViolation::new("/date_end", "date_range_limit", "shorten_date_range")
                        .with_length(31, days as usize, "days"),
                );
            }
        }
    }
    if let (Some(min), Some(max)) = (args["min_size"].as_u64(), args["max_size"].as_u64())
        && min > max
    {
        let mut issue = InputViolation::new("/max_size", "conflicting_range", "adjust_range");
        issue.related_field = Some("/min_size");
        push(out, issue);
    }
    if name == "locate_message" {
        let id = args["message_id"]
            .as_str()
            .expect("validated required field")
            .trim();
        if id.len() < 5
            || !id.is_ascii()
            || !id.starts_with('<')
            || !id.ends_with('>')
            || !id.contains('@')
            || id[1..id.len().saturating_sub(1)]
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || c == '<' || c == '>')
        {
            push(
                out,
                InputViolation::new("/message_id", "invalid_message_id", "use_message_id_header"),
            );
        }
    }
}

fn valid_date(text: &str, minimum_year: u32) -> bool {
    let b = text.as_bytes();
    if b.len() != 10
        || b[4] != b'-'
        || b[7] != b'-'
        || !b
            .iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit())
    {
        return false;
    }
    let year: u32 = text[..4].parse().unwrap_or(0);
    let month: usize = text[5..7].parse().unwrap_or(0);
    let day: u32 = text[8..].parse().unwrap_or(0);
    if year < minimum_year || !(1..=12).contains(&month) || day == 0 {
        return false;
    }
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    day <= [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ][month - 1]
}

fn date_ordinal(text: &str) -> u32 {
    let year: u32 = text[..4].parse().expect("validated date");
    let month: usize = text[5..7].parse().expect("validated date");
    let day: u32 = text[8..].parse().expect("validated date");
    let prior = year - 1;
    let leap = year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400));
    prior * 365 + prior / 4 - prior / 100
        + prior / 400
        + [
            31,
            if leap { 29 } else { 28 },
            31,
            30,
            31,
            30,
            31,
            31,
            30,
            31,
            30,
            31,
        ][..month - 1]
            .iter()
            .sum::<u32>()
        + day
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(name: &str) -> Value {
        serde_json::to_value(
            crate::server::tools("full")
                .into_iter()
                .find(|t| t.name == name)
                .unwrap()
                .input_schema,
        )
        .unwrap()
    }
    fn check(name: &str, args: Value) -> Result<()> {
        validate(name, &args, &schema(name))
    }
    fn details(error: MailError) -> Value {
        serde_json::to_value(error).unwrap()["details"].clone()
    }

    #[test]
    fn read_lengths_count_unicode_code_points_and_publish_byte_limits() {
        for text in [
            "a".repeat(512),
            "中".repeat(512),
            "😀".repeat(512),
            "e\u{301}".repeat(256),
        ] {
            assert_eq!(text.chars().count(), 512);
            for field in ["query", "from", "to", "subject", "mailbox"] {
                check("search_emails", json!({field:text})).unwrap();
                let bad =
                    details(check("search_emails", json!({field:format!("{text}x")})).unwrap_err());
                assert_eq!(bad["field"], format!("/{field}"));
                assert_eq!(bad["reason"], "character_limit");
                assert_eq!(bad["actual"], 513);
                assert_eq!(bad["unit"], "unicode_code_points");
            }
            check(
                "locate_message",
                json!({"message_id":"<a@b>","mailboxes":[text]}),
            )
            .unwrap();
        }
        let properties = &schema("search_emails")["properties"];
        for field in ["mailbox", "query", "from", "to", "subject"] {
            assert_eq!(properties[field]["maxLength"], 512);
            assert_eq!(properties[field]["x-maxUtf8Bytes"], 2048);
        }
        assert_eq!(properties["mailbox"]["x-maxModifiedUtf7Bytes"], 8192);
        let mut out = Vec::new();
        structural(
            &json!("😀😀"),
            &json!({"type":"string","maxLength":2,"x-maxUtf8Bytes":7}),
            "/field",
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].reason, "utf8_byte_limit");
        assert_eq!(out[0].actual, Some(8));
    }

    #[test]
    fn read_structure_errors_are_bounded_private_and_point_to_array_items() {
        let bad = details(check("get_emails",json!({"uids":["private-body",false,-1],"uid_validity":"private-query","private-unknown-key":"private-value"})).unwrap_err());
        assert_eq!(bad["reason"], "unknown_field");
        assert_eq!(bad["field"], "");
        let rest = bad["additional_violations"].as_array().unwrap();
        assert!(rest.iter().any(|e| e["field"] == "/uids/0"
            && e["expected"] == "integer"
            && e["actual_type"] == "string"));
        assert!(
            rest.iter()
                .any(|e| e["field"] == "/uids/2" && e["reason"] == "integer_range")
        );
        let text = bad.to_string();
        for private in [
            "private-body",
            "private-query",
            "private-unknown-key",
            "private-value",
        ] {
            assert!(!text.contains(private));
        }
        let bad = details(
            check(
                "get_emails",
                json!({"uids":vec!["private";25],"uid_validity":0}),
            )
            .unwrap_err(),
        );
        assert_eq!(bad["additional_violations"].as_array().unwrap().len(), 7);
        let bad = details(
            check(
                "get_email",
                json!({"uid":1,"uid_validity":2,"body_mode":"private"}),
            )
            .unwrap_err(),
        );
        assert_eq!(bad["field"], "/body_mode");
        assert_eq!(bad["reason"], "unsupported_value");
        let bad = details(check("get_email", json!({"uid":1})).unwrap_err());
        assert_eq!(bad["field"], "/uid_validity");
        assert_eq!(bad["reason"], "required_field");
        assert_eq!(pointer("/items/1", "a~/b"), "/items/1/a~0~1b");
    }

    #[test]
    fn semantic_errors_cover_dates_ranges_message_ids_and_controls() {
        check(
            "search_emails",
            json!({"since":"2024-02-29","before":"2024-03-01","min_size":0,"max_size":1}),
        )
        .unwrap();
        for (args, field, reason) in [
            (json!({"since":"2023-02-29"}), "/since", "invalid_date"),
            (json!({"since":"0000-01-01"}), "/since", "invalid_date"),
            (
                json!({"since":"2024-03-01","before":"2024-03-01"}),
                "/before",
                "conflicting_range",
            ),
            (
                json!({"min_size":9,"max_size":8}),
                "/max_size",
                "conflicting_range",
            ),
            (
                json!({"query":"private\ntext"}),
                "/query",
                "forbidden_control_character",
            ),
        ] {
            let bad = details(check("search_emails", args).unwrap_err());
            assert_eq!(bad["field"], field);
            assert_eq!(bad["reason"], reason);
        }
        check(
            "get_mailbox_statistics",
            json!({"date_start":"2024-02-01","date_end":"2024-03-03"}),
        )
        .unwrap();
        for (args, field, reason) in [
            (
                json!({"date_start":"2024-01-01"}),
                "/date_end",
                "required_with_field",
            ),
            (
                json!({"date_start":"2024-02-01","date_end":"2024-03-04"}),
                "/date_end",
                "date_range_limit",
            ),
            (
                json!({"date_start":"1969-01-01","date_end":"1970-01-01"}),
                "/date_start",
                "invalid_date",
            ),
        ] {
            let bad = details(check("get_mailbox_statistics", args).unwrap_err());
            assert_eq!(bad["field"], field);
            assert_eq!(bad["reason"], reason);
        }
        let bad = details(
            check(
                "locate_message",
                json!({"message_id":"<private>","mailboxes":["INBOX"]}),
            )
            .unwrap_err(),
        );
        assert_eq!(bad["reason"], "invalid_message_id");
        for (value, reason) in [
            ("", "character_minimum"),
            ("bad\nname", "forbidden_control_character"),
            ("bad\tname", "forbidden_control_character"),
        ] {
            let bad = details(
                check(
                    "locate_message",
                    json!({"message_id":"<a@b>","mailboxes":["INBOX",value]}),
                )
                .unwrap_err(),
            );
            assert_eq!(bad["field"], "/mailboxes/1");
            assert_eq!(bad["reason"], reason);
        }
        for value in ["private\0text", "private\rtext"] {
            let bad = details(check("search_emails", json!({"query":value})).unwrap_err());
            assert_eq!(bad["reason"], "forbidden_control_character");
            assert!(!bad.to_string().contains("private"));
        }
    }

    #[test]
    fn operation_lookup_keeps_legacy_key_byte_budget() {
        check("get_operation", json!({"operation_key":"中".repeat(42)})).unwrap();
        let bad =
            details(check("get_operation", json!({"operation_key":"中".repeat(43)})).unwrap_err());
        assert_eq!(bad["reason"], "utf8_byte_limit");
        assert_eq!(bad["limit"], 128);
    }

    #[test]
    fn attachment_selectors_publish_and_enforce_the_same_reference_rules() {
        let id = format!("att_v1_{}", "a".repeat(64));
        for name in ["download_attachment", "prepare_attachment_download"] {
            let input = schema(name);
            let standard = jsonschema::draft202012::options().build(&input).unwrap();
            for selector in [
                json!({"attachment_id":id}),
                json!({"attachment_index":0}),
                json!({"attachment_index":1023}),
                json!({"attachment_id":id,"attachment_index":1023}),
            ] {
                let mut args = json!({"uid":7,"uid_validity":9,"account_id":"default"});
                args.as_object_mut()
                    .unwrap()
                    .extend(selector.as_object().unwrap().clone());
                assert!(standard.is_valid(&args));
                check(name, args).unwrap();
            }
            for selector in [
                json!({}),
                json!({"attachment_index":1024}),
                json!({"attachment_index":-1}),
                json!({"attachment_id":format!("att_v1_{}","A".repeat(64))}),
                json!({"attachment_id":null}),
            ] {
                let mut args = json!({"uid":7,"uid_validity":9});
                args.as_object_mut()
                    .unwrap()
                    .extend(selector.as_object().unwrap().clone());
                assert!(!standard.is_valid(&args));
                assert!(check(name, args).is_err());
            }
        }
        check(
            "list_attachments",
            json!({"uid":7,"uid_validity":9,"attachment_offset":1023}),
        )
        .unwrap();
        assert!(
            check(
                "list_attachments",
                json!({"uid":7,"uid_validity":9,"attachment_offset":1024})
            )
            .is_err()
        );
    }

    #[test]
    fn read_account_aliases_share_catalog_syntax_and_never_extend_write_or_journal_inputs() {
        let schema = schema("get_email");
        let property = &schema["properties"]["account_id"];
        let standard = jsonschema::draft202012::options().build(property).unwrap();
        for alias in [
            "default".to_owned(),
            "Billing-A.1".to_owned(),
            "a".repeat(ACCOUNT_ID_MAX_CHARACTERS),
        ] {
            assert!(valid_account_id(&alias));
            assert!(standard.is_valid(&json!(alias)));
            check(
                "get_email",
                json!({"account_id":alias,"uid":7,"uid_validity":9}),
            )
            .unwrap();
        }
        for alias in [
            "".to_owned(),
            "private account".to_owned(),
            "账户".to_owned(),
            "_private".to_owned(),
            "a".repeat(ACCOUNT_ID_MAX_CHARACTERS + 1),
        ] {
            assert!(!valid_account_id(&alias));
            assert!(!standard.is_valid(&json!(alias)));
            let error = check(
                "get_email",
                json!({"account_id":alias,"uid":7,"uid_validity":9}),
            )
            .unwrap_err();
            assert!(
                !serde_json::to_string(&error)
                    .unwrap()
                    .contains("private account")
            );
        }
        assert!(
            check(
                "get_operation",
                json!({"operation_key":"old","account_id":"default"})
            )
            .is_err()
        );
    }
}
