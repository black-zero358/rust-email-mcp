//! Exact structured-content contracts for the currently implemented read tools.
//! Success, local item errors and the complete-wire fallback share this source.
use crate::accounts::ACCOUNT_ID_MAX_CHARACTERS;
use crate::mail::attachments::MAX_ATTACHMENT_INDEX;
use serde_json::{Value, json};

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}
fn all_required(properties: Value) -> Value {
    let keys = properties
        .as_object()
        .expect("static properties")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    json!({"type":"object","properties":properties,"required":keys,"additionalProperties":false})
}
fn string() -> Value {
    json!({"type":"string"})
}
fn boolean() -> Value {
    json!({"type":"boolean"})
}
fn count() -> Value {
    json!({"type":"integer","minimum":0})
}
fn uid() -> Value {
    json!({"type":"integer","minimum":1,"maximum":4294967295_u64})
}
fn nullable(value: Value) -> Value {
    json!({"anyOf":[value,{"type":"null"}]})
}
fn array(items: Value) -> Value {
    json!({"type":"array","items":items})
}
fn reference(name: &str) -> Value {
    json!({"$ref":format!("#/$defs/{name}")})
}
fn enumeration(values: &[&str]) -> Value {
    json!({"type":"string","enum":values})
}
fn account_id() -> Value {
    json!({"type":"string","minLength":1,"maxLength":ACCOUNT_ID_MAX_CHARACTERS,"pattern":format!("^[A-Za-z0-9][A-Za-z0-9._-]{{0,{}}}$",ACCOUNT_ID_MAX_CHARACTERS-1)})
}

fn account_context(mut schema: Value, mailbox: bool) -> Value {
    if let Some(branches) = schema.get_mut("oneOf").and_then(Value::as_array_mut) {
        for branch in branches {
            *branch = account_context(branch.clone(), mailbox);
        }
        return schema;
    }
    if schema.get("$ref").is_some() {
        return schema;
    }
    schema["properties"]["account_id"] = account_id();
    if mailbox {
        schema["properties"]["mailbox"] = string();
    }
    let required = schema["required"]
        .as_array_mut()
        .expect("concrete read object");
    for key in if mailbox {
        &["account_id", "mailbox"][..]
    } else {
        &["account_id"][..]
    } {
        if !required.iter().any(|value| value == key) {
            required.push(json!(key));
        }
    }
    schema
}

fn violation() -> Value {
    object(
        json!({
            "field":{"type":"string","pattern":"^(?:/(?:[^~/]|~[01])*)*$"},
            "reason":enumeration(&["type_mismatch","required_field","unknown_field","character_minimum","character_limit","utf8_byte_limit","wire_byte_limit","forbidden_control_character","integer_range","item_count","constant_mismatch","unsupported_value","invalid_date","conflicting_range","required_with_field","date_range_limit","invalid_message_id","resource_limit","attachment_reference_required","invalid_attachment_id","reference_mismatch","invalid_account_id"]),
            "action":enumeration(&["use_expected_type","provide_field","remove_unknown_fields","provide_longer_field","shorten_field","remove_control_characters","use_expected_range","use_expected_item_count","use_required_value","use_supported_value","use_valid_calendar_date","adjust_range","provide_paired_field","shorten_date_range","use_message_id_header","reduce_request","provide_attachment_reference","relist_attachments","use_matching_reference","use_default_account","list_accounts"]),
            "expected":string(),"actual_type":enumeration(&["null","boolean","integer","number","string","array","object"]),
            "limit":count(),"actual":count(),"unit":enumeration(&["unicode_code_points","utf8_bytes","modified_utf7_bytes","items","days","bytes"]),
            "minimum":count(),"maximum":count(),"related_field":{"type":"string","pattern":"^(?:/(?:[^~/]|~[01])*)*$"}
        }),
        &["field", "reason", "action"],
    )
}
fn error() -> Value {
    let mut details = violation();
    let properties = details["properties"].as_object_mut().unwrap();
    properties.insert(
        "additional_violations".into(),
        json!({"type":"array","items":reference("violation"),"maxItems":7}),
    );
    properties.insert(
        "limit_kind".into(),
        enumeration(&[
            "raw_message_bytes",
            "decoded_attachment_bytes",
            "decoded_body_bytes",
            "mime_structure",
            "header_bytes",
            "wire_response_bytes",
            "scan_items",
            "cache_quota",
        ]),
    );
    // A limit diagnostic can contain only limit_kind; field diagnostics always
    // contain the complete primary violation. Optional details may be dropped.
    details.as_object_mut().unwrap().remove("required");
    details["anyOf"] = json!([{"required":["field","reason","action"]},{"required":["limit_kind"],"not":{"anyOf":[{"required":["field"]},{"required":["reason"]},{"required":["action"]}]}}]);
    let mut dependencies = json!({});
    for key in [
        "additional_violations",
        "expected",
        "actual_type",
        "limit",
        "actual",
        "unit",
        "minimum",
        "maximum",
        "related_field",
    ] {
        dependencies[key] = json!(["field", "reason", "action"]);
    }
    details["dependentRequired"] = dependencies;
    object(
        json!({
            "code":enumeration(&["INVALID_INPUT","MAIL_BACKEND","STORAGE_UNAVAILABLE","LIMIT_EXCEEDED","TIMEOUT","BUSY","STALE_UID","NOT_FOUND","UNSUPPORTED_BODY","UNSUPPORTED_CHARSET","UNSUPPORTED_TRANSFER_ENCODING","ATTACHMENT_DECODE_FAILED","ATTACHMENT_NOT_FOUND","ACCOUNT_UNAVAILABLE","INTERNAL"]),
            "message":string(),"retryable":boolean(),"details":details
        }),
        &["code", "message", "retryable"],
    )
}

fn message(conversation: bool) -> Value {
    let mut properties = json!({
        "account_id":account_id(),"mailbox":string(),
        "uid":uid(),"uid_validity":nullable(uid()),"subject":nullable(string()),"from":nullable(string()),"to":nullable(string()),
        "body":string(),"body_mode":enumeration(&["full","trimmed"]),"body_truncated":boolean(),"body_omitted":boolean(),
        "quoted_content_omitted":boolean(),"signature_omitted":boolean(),"body_original_bytes":count(),"body_byte_limit":count(),
        "message_id":nullable(string()),"references":nullable(string()),"in_reply_to":nullable(string()),
        "attachments":array(all_required(json!({"attachment_index":count(),"content_type":string(),"filename":nullable(string())}))),
        "content_trust":{"const":"untrusted"},
        "html_display_metadata":all_required(json!({
            "format_version":enumeration(&["legacy-v1","readable-v1"]),"html_parts":count(),"omitted":boolean(),
            "omissions":{"type":"array","maxItems":8,"items":all_required(json!({"reason":enumeration(&["link_scheme","link_relative","link_control","link_length","link_limit","nesting_limit","image_source","malformed_structure"]),"count":{"type":"integer","minimum":1}}))}
        }))
    });
    let mut schema = all_required(properties.clone());
    if conversation {
        properties["attachments"]["maxItems"] = json!(0);
        properties["attachment_metadata_omitted"] = boolean();
        properties["metadata_fields_omitted"] = json!({"type":"array","maxItems":6,"items":enumeration(&["subject","from","to","message_id","references","in_reply_to"])});
        schema["properties"] = properties;
        schema["required"].as_array_mut().unwrap().extend([
            json!("attachment_metadata_omitted"),
            json!("metadata_fields_omitted"),
        ]);
    }
    schema
}

fn operation() -> Value {
    all_required(
        json!({"status":enumeration(&["sending","accepted","rejected","unknown"]),"message_id":string(),"warnings":array(string())}),
    )
}

fn attachment_reference() -> Value {
    json!({"account_id":account_id(),"mailbox":string(),"uid":uid(),"uid_validity":uid(),
        "attachment_id":{"type":"string","pattern":"^att_v1_[0-9a-f]{64}$"},"attachment_index":{"type":"integer","minimum":0,"maximum":MAX_ATTACHMENT_INDEX}})
}
fn checksum() -> Value {
    all_required(
        json!({"algorithm":{"const":"sha256"},"value":{"type":"string","pattern":"^[0-9a-f]{64}$"}}),
    )
}
fn delivery() -> Value {
    all_required(json!({
        "available_modes":{"type":"array","items":enumeration(&["inline_base64","authenticated_http"]),"maxItems":2,"uniqueItems":true},
        "authentication":{"const":"same_subject_bearer"},"expires_in_seconds":{"const":600},"expires_at":nullable(count()),
        "client_delivery_required":{"const":true},"next_action":enumeration(&["prepare_attachment_download","authenticated_get_then_import_file","use_supported_attachment"])
    }))
}
fn attachment_descriptor() -> Value {
    let mut properties = attachment_reference();
    properties.as_object_mut().unwrap().extend(json!({
        "filename":nullable(string()),"safe_filename":string(),"content_type":string(),"declared_content_type":string(),"detected_content_type":{"type":"null"},
        "encoded_size":count(),"decoded_size":nullable(count()),"size_accuracy":enumeration(&["exact","estimate","unknown"]),
        "disposition":nullable(string()),"content_id":nullable(string()),"downloadable":boolean(),
        "unavailable_reason":nullable(enumeration(&["unsupported_transfer_encoding","decoded_attachment_bytes"])),
        "delivery":delivery(),"checksum":nullable(checksum())
    }).as_object().unwrap().clone());
    all_required(properties)
}
fn read_diagnostics() -> Value {
    all_required(json!({
        "source":enumeration(&["bodystructure","raw"]),
        "fallback_reason":nullable(enumeration(&["structure_limit","structure_rejected","mapping_unverified","metadata_conflict","whole_multipart_attachment","header_budget"])),
        "limit_kind":nullable(enumeration(&["mime_structure"]))
    }))
}

fn accounts() -> Value {
    let capabilities = all_required(json!({
        "attachment_delivery_modes":{"const":["inline_base64","authenticated_http"]},"part_fetch":{"const":false},
        "cross_mailbox_conversation":{"const":false},"cache_modes":{"const":["off"]}
    }));
    let mut limits = json!({});
    for name in [
        "mailbox_characters",
        "query_characters",
        "text_utf8_bytes",
        "mailbox_modified_utf7_bytes",
        "raw_message_bytes",
        "connection_bytes",
        "wire_response_bytes",
        "inline_attachment_decoded_bytes",
        "batch_read_items",
        "list_page_items",
        "scan_uid_window",
        "execution_slots",
        "admission_requests",
        "per_account_base_slots",
        "background_slots",
        "queue_wait_seconds",
        "backend_backoff_seconds",
        "operation_timeout_seconds",
    ] {
        limits[name] = count();
    }
    all_required(
        json!({"default_account_id":nullable(account_id()),"accounts":{"type":"array","items":all_required(json!({
        "id":account_id(),"display_name":string(),"address":string(),"mode":enumeration(&["read","draft","send","full"]),"default":boolean(),"capabilities":capabilities,"limits":all_required(limits)
        }))}}),
    )
}

fn data(name: &str) -> Value {
    let trust = json!({"const":"untrusted"});
    let roles = || {
        array(enumeration(&[
            "inbox", "all", "archive", "drafts", "flagged", "junk", "sent", "trash",
        ]))
    };
    let schema = match name {
        "list_accounts" => accounts(),
        "list_mailboxes" => all_required(
            json!({"mailboxes":{"type":"array","maxItems":200,"items":all_required(json!({
                "name":string(),"attributes":string(),"attribute_flags":array(string()),"delimiter":nullable(string()),
                "selectable":boolean(),"special_use":roles(),"role_source":nullable(enumeration(&["server","configuration","protocol"])),"role_candidates":roles()
            }))}}),
        ),
        "list_emails" | "search_emails" => object(
            json!({
                "emails":{"type":"array","maxItems":50,"items":all_required(json!({"account_id":account_id(),"mailbox":string(),"uid_validity":nullable(uid()),"uid":uid(),"subject":nullable(string()),"from":nullable(string()),"date":nullable(string()),"size":nullable(count()),"flags":array(string())}))},
                "next_before_uid":nullable(uid()),"uid_validity":nullable(uid()),
                "scanned_uid_range":{"type":"array","prefixItems":[uid(),uid()],"minItems":2,"maxItems":2}
            }),
            &["emails", "next_before_uid", "uid_validity"],
        ),
        "get_email" => reference("message"),
        "get_email_status" => all_required(
            json!({"uid":uid(),"uid_validity":nullable(uid()),"flags":array(string())}),
        ),
        "get_emails" => all_required(json!({
            "uid_validity":nullable(uid()),
            "items":{"type":"array","minItems":1,"maxItems":10,"items":{"oneOf":[
                all_required(json!({"uid":uid(),"ok":{"const":true},"email":reference("message")})),
                all_required(json!({"uid":uid(),"ok":{"const":false},"error":reference("error")})),
                all_required(json!({"uid":uid(),"ok":{"const":false},"status":{"const":"unprocessed"}}))
            ]}},"requested":{"type":"integer","minimum":1,"maximum":10},"completed":{"type":"integer","minimum":0,"maximum":10}
        })),
        "download_attachment" => {
            let mut properties = attachment_reference();
            properties.as_object_mut().unwrap().extend(json!({"content_type":string(),"size":count(),"base64":{"type":"string","contentEncoding":"base64"},"checksum":checksum()}).as_object().unwrap().clone());
            all_required(properties)
        }
        "list_attachments" => {
            let mut schema = all_required(
                json!({"account_id":account_id(),"mailbox":string(),"uid":uid(),"uid_validity":uid(),"attachments":array(attachment_descriptor()),"next_attachment_offset":nullable(json!({"type":"integer","minimum":0,"maximum":MAX_ATTACHMENT_INDEX})),"content_trust":trust}),
            );
            schema["properties"]["read_diagnostics"] = read_diagnostics();
            schema
        }
        "prepare_attachment_download" => {
            let mut properties = attachment_reference();
            properties.as_object_mut().unwrap().extend(json!({"download_url":{"type":"string","format":"uri"},"method":{"const":"GET"},
                "required_headers":all_required(json!({"Authorization":{"const":"Bearer <access_token>"}})),"expires_in_seconds":{"const":600},
                "metadata":attachment_descriptor(),"delivery":delivery(),"content_trust":trust
            }).as_object().unwrap().clone());
            let mut schema = all_required(properties);
            schema["properties"]["read_diagnostics"] = read_diagnostics();
            schema
        }
        "get_operation" => all_required(json!({"operation":nullable(reference("operation"))})),
        "read_conversation" => all_required(json!({
            "mailbox":string(),"uid_validity":uid(),"messages":{"type":"array","maxItems":20,"items":reference("conversation_message")},"omitted_uids":array(uid()),
            "threading":{"const":"Message-ID/References/In-Reply-To exact identifier graph; no subject matching"},
            "scope":all_required(json!({"start_uid":uid(),"end_uid":count(),"candidates_checked":count(),"seed_uid":uid()})),
            "next_before_uid":nullable(uid()),"candidate_limit_reached":boolean(),"messages_omitted":boolean(),"response_budget_omitted":boolean(),
            "older_uids_omitted":boolean(),"total_body_byte_limit":count(),"skipped_candidates":array(reference("uid_error")),"message_errors":array(reference("uid_error")),"complete":boolean(),"content_trust":trust
        })),
        "locate_message" => all_required(json!({
            "matches":{"type":"array","maxItems":50,"items":all_required(json!({"account_id":account_id(),"mailbox":string(),"uid":uid(),"uid_validity":uid(),"message_id":string()}))},
            "scopes":{"type":"array","minItems":1,"maxItems":20,"items":all_required(json!({"account_id":account_id(),"mailbox":string(),"uid_validity":uid(),"start_uid":uid(),"end_uid":count(),"next_before_uid":nullable(uid()),"candidates_checked":count(),"candidate_limit_reached":boolean(),"older_uids_omitted":boolean()}))},
            "complete":{"const":false},"scope":{"const":"Only explicitly supplied folders and reported UID windows were searched; follow each cursor."}
        })),
        "get_mailbox_statistics" => {
            let mut schema = object(
                json!({"mailbox":string(),"messages":count(),"unseen":count(),"recent":count(),"uid_validity":count(),"uid_next":count(),"snapshot_atomic":{"const":false},
                    "distribution":{"type":"array","minItems":1,"maxItems":31,"items":all_required(json!({"date":{"type":"string","format":"date"},"messages":count()}))},
                    "distribution_scope":all_required(json!({"basis":{"const":"IMAP INTERNALDATE server calendar day"},"date_end_exclusive":{"type":"string","format":"date"},"start_uid":uid(),"end_uid":count(),"older_uids_omitted":boolean(),"next_before_uid":nullable(uid())}))
                }),
                &[
                    "mailbox",
                    "messages",
                    "unseen",
                    "recent",
                    "uid_validity",
                    "uid_next",
                    "snapshot_atomic",
                ],
            );
            schema["dependentRequired"] = json!({"distribution":["distribution_scope"],"distribution_scope":["distribution"]});
            schema
        }
        "diagnose_connectivity" => {
            let common = json!({"ok":boolean(),"stages":{"type":"array","minItems":1,"maxItems":7,"items":all_required(json!({"stage":enumeration(&["mcp_auth","tcp","tls","greeting","login","capability"]),"status":enumeration(&["passed","failed"])}))},"service":{"const":"imap"},"smtp_tested":{"const":false}});
            let mut success = common.clone();
            success["ok"] = json!({"const":true});
            success["capabilities"] = all_required(
                json!({"move":boolean(),"uidplus":boolean(),"imap4rev1":boolean(),"imap4rev2":boolean()}),
            );
            let mut failed = common;
            failed["ok"] = json!({"const":false});
            failed["failure"] = enumeration(&["connection_check_failed", "timeout"]);
            json!({"oneOf":[all_required(success),all_required(failed)]})
        }
        _ => unreachable!("only read tools have output schemas"),
    };
    if super::is_account_read(name) {
        account_context(
            schema,
            !matches!(
                name,
                "list_mailboxes" | "locate_message" | "diagnose_connectivity"
            ),
        )
    } else {
        schema
    }
}

pub(super) fn schema(name: &str) -> Value {
    let success = all_required(json!({"data":data(name),"request_id":string()}));
    let failure = all_required(json!({"error":reference("error"),"request_id":string()}));
    let mut fallback_properties = json!({"error":{"allOf":[reference("error"),{"properties":{"code":{"const":"LIMIT_EXCEEDED"}}}]},"request_id":string(),"recovery":{"const":"Inspect get_operation with the original operation_key before retrying a write."},"retry_allowed":{"const":false}});
    if name == "get_operation" {
        fallback_properties["operation"] = all_required(
            json!({"status":enumeration(&["sending","accepted","rejected","unknown"])}),
        );
    }
    let fallback = object(
        fallback_properties,
        &["error", "request_id", "recovery", "retry_allowed"],
    );
    json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema","type":"object",
        "oneOf":[success,failure,fallback],
        "$defs":{"error":error(),"violation":violation(),"message":message(false),"conversation_message":message(true),"operation":operation(),"uid_error":all_required(json!({"uid":uid(),"error":reference("error")}))}
    })
}
