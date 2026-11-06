//! Exact structured-content contracts for read and message organization tools.
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

fn layer_coverage() -> Value {
    let layer = all_required(json!({
        "materialized":count(),"materialized_bytes":count(),"derivable_from_raw":count(),
        "missing":count(),"omitted":count(),"unsupported":count()
    }));
    all_required(json!({"raw":layer,"full_body":layer,"attachments":layer}))
}

fn sync_status() -> Value {
    let scan = all_required(json!({
        "scan_id":string(),"high_uid":count(),"revision":count(),"next_low":nullable(uid()),
        "known_after":nullable(count()),"checked_at_ms":count(),"completed_at_ms":nullable(count()),
        "complete":boolean()
    }));
    let layer = all_required(json!({
        "materialized":count(),"materialized_bytes":count(),"derivable_from_raw":count(),
        "missing":count(),"omitted":count(),"unsupported":count(),
        "available_complete":boolean(),"materialization_complete":boolean()
    }));
    let coverage = all_required(json!({
        "high_uid":count(),"discovery_complete":boolean(),"existence_flags_verified":boolean(),
        "known_messages":count(),"complete_listing_facts":count(),"complete_headers":count(),
        "partial_headers":count(),"missing_headers":count(),"indexed_headers":count(),
        "exact_thread_index":count(),"verified_manifests":count(),"known_flags":count(),
        "flags_checked_at_ms":nullable(count()),"indexed_bodies":count(),
        "listing_facts_complete":boolean(),"headers_complete":boolean(),
        "exact_thread_index_complete":boolean(),"manifests_complete":boolean(),
        "body_index_complete":boolean(),"raw":layer,"full_body":layer,"attachments":layer,
        "work":all_required(json!({
            "pending":count(),"completed":count(),"terminal":count(),"quota_blocked":count(),
            "retrying":count(),"next_due_ms":nullable(count())
        })),
        "gaps":array(all_required(json!({"layer":string(),"status":string(),"reason":string(),"count":count()})))
    }));
    let scope = all_required(json!({
        "uid_validity":nullable(uid()),"observed_high_uid":nullable(count()),
        "checked_at_ms":nullable(count()),"dirty":boolean(),"history":nullable(scan.clone()),
        "tail":nullable(scan.clone()),"reconciliation":nullable(scan),
        "history_coverage":nullable(coverage.clone()),"current_coverage":nullable(coverage)
    }));
    let lane = nullable(all_required(json!({
        "uid_validity":uid(),"high_uid":count(),"committed_modseq":string(),
        "target_modseq":nullable(string()),"known_after":nullable(count()),"checked_at_ms":count()
    })));
    let folder = all_required(json!({
        "mailbox":string(),"progress":nullable(scope),
        "error":nullable(enumeration(&["quota","storage","verification_pending"])),
        "condstore":lane.clone(),"qresync":lane,
        "runtime":all_required(json!({
            "last_error":nullable(enumeration(&["network","protocol","storage","quota"])),
            "quota_paused":boolean(),
            "protocol":enumeration(&["rev1","condstore","qresync"]),
            "protocol_fallback":nullable(enumeration(&["capability_unavailable","extension_failed","nomodseq","modseq_rollback","qresync_enable_failed","qresync_failed","qresync_reconciliation_required"]))
        }))
    }));
    all_required(json!({
        "enabled":boolean(),"interval_seconds":count(),"reconcile_interval_seconds":count(),
        "protocol":enumeration(&["rev1","auto"]),
        "running":boolean(),"folders":nullable(json!({"type":"array","maxItems":64,"items":folder}))
    }))
}

fn cache_context() -> Value {
    let coverage = object(
        json!({
            "remote_complete":{"const":false},"known_messages":count(),"complete_headers":count(),
            "indexed_threads":count(),"list_observed":boolean(),"filter_unknown":count(),"headers_missing":count(),
            "search_index":all_required(json!({"indexed_metadata":count(),"indexed_body":count(),
                "version":{"const":crate::cache::BODY_INDEX_VERSION},"rebuild_pending":boolean()})),
            "layers":layer_coverage(),
        }),
        &["remote_complete"],
    );
    let scope = object(
        json!({
            "mailbox":string(),"checked_at_ms":nullable(count()),"uid_validity":uid(),"high_uid":count(),
            "coverage":coverage,
        }),
        &["mailbox", "checked_at_ms", "coverage"],
    );
    all_required(json!({
        "stale":{"const":true},"remote_generation_unverified":{"const":true},
        "scopes":{"type":"array","maxItems":64,"items":scope}
    }))
}

fn source_properties(schema: &mut Value, field: &str, default: &str) {
    schema["properties"][field] = enumeration(&[default, "cache"]);
    schema["properties"]["cache"] = cache_context();
    schema["dependentRequired"]["cache"] = json!([field]);
    schema["allOf"] = json!([{
        "if":{"properties":{field:{"const":"cache"}},"required":[field]},
        "then":{"required":["cache"]},
        "else":{"not":{"required":["cache"]}}
    }]);
}

fn cross_conversation() -> Value {
    let message_ref = all_required(
        json!({"account_id":account_id(),"mailbox":string(),"uid_validity":uid(),"uid":uid()}),
    );
    let variant = object(
        json!({
            "variant_id":string(),"representative":message_ref,"locations":array(message_ref.clone()),"new_variant":boolean(),
            "message":reference("conversation_message"),"body_error":reference("error"),
        }),
        &["variant_id", "representative", "locations", "new_variant"],
    );
    let entity = all_required(json!({
        "entity_id":string(),"message_id":nullable(string()),"identity_status":enumeration(&["identity_unverified","verified","collision"]),
        "new_entity":boolean(),"variants":array(variant)
    }));
    let folder = all_required(json!({
        "mailbox":string(),"uid_validity":nullable(uid()),"high_uid":nullable(count()),"completed_windows":count(),"remaining_windows":count(),
        "pending_candidates":count(),"unresolved_errors":count(),"complete":boolean(),"error":nullable(reference("error"))
    }));
    let coverage = all_required(json!({
        "completed_windows":count(),"remaining_windows":count(),"pending_candidates":count(),"unresolved_errors":count(),"known_locations":count(),
        "graph_complete":boolean(),"delivery_pending":boolean(),"body_requested":count(),"body_returned":count()
    }));
    let errors = json!({"oneOf":[
        all_required(json!({"stage":{"const":"folder"},"mailbox":string(),"error":reference("error")})),
        all_required(json!({"stage":{"const":"search"},"mailbox":string(),"field":enumeration(&["Message-ID","References","In-Reply-To"]),
            "start_uid":uid(),"end_uid":uid(),"error":reference("error")})),
        all_required(json!({"stage":{"const":"header"},"ref":message_ref,"error":reference("error")})),
        all_required(json!({"stage":{"const":"cursor"},"error":reference("error")}))
    ]});
    let mut schema = object(
        json!({
            "mode":{"const":"cross_mailbox"},"account_id":account_id(),"seed":message_ref,
            "folders":{"type":"array","minItems":1,"maxItems":20,"items":folder},
            "messages":{"type":"array","maxItems":20,"items":entity},"cursor":nullable(json!({"type":"string","format":"uuid"})),
            "cursor_storage":enumeration(&["memory","persistent"]),
            "coverage":coverage,"errors":{"type":"array","maxItems":20,"items":errors},"errors_omitted":count(),"omitted_refs":array(message_ref),
            "complete":boolean(),"response_budget_omitted":boolean(),"content_trust":{"const":"untrusted"},
            "cursor_action":enumeration(&["resume_or_restart_with_narrower_scope","restart_later_or_with_narrower_scope"]),
        }),
        &[
            "mode",
            "account_id",
            "seed",
            "folders",
            "messages",
            "cursor",
            "cursor_storage",
            "coverage",
            "errors",
            "errors_omitted",
            "omitted_refs",
            "complete",
            "response_budget_omitted",
            "content_trust",
        ],
    );
    source_properties(&mut schema, "read_source", "live");
    schema
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
            "reason":enumeration(&["type_mismatch","required_field","unknown_field","character_minimum","character_limit","utf8_byte_limit","wire_byte_limit","forbidden_control_character","integer_range","item_count","constant_mismatch","unsupported_value","invalid_date","conflicting_range","required_with_field","date_range_limit","invalid_message_id","resource_limit","attachment_reference_required","invalid_attachment_id","reference_mismatch","invalid_account_id","conflicting_field"]),
            "action":enumeration(&["use_expected_type","provide_field","remove_unknown_fields","provide_longer_field","shorten_field","remove_control_characters","use_expected_range","use_expected_item_count","use_required_value","use_supported_value","use_valid_calendar_date","adjust_range","provide_paired_field","shorten_date_range","use_message_id_header","reduce_request","provide_attachment_reference","relist_attachments","use_matching_reference","use_default_account","list_accounts","remove_conflicting_field"]),
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
            "connection_bytes",
            "part_fetch_bytes",
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
            "code":enumeration(&["INVALID_INPUT","MAIL_BACKEND","STORAGE_UNAVAILABLE","ATTACHMENT_STORAGE_UNAVAILABLE","LIMIT_EXCEEDED","TIMEOUT","BUSY","STALE_UID","NOT_FOUND","UNSUPPORTED","UNSUPPORTED_BODY","UNSUPPORTED_CHARSET","UNSUPPORTED_TRANSFER_ENCODING","ATTACHMENT_DECODE_FAILED","ATTACHMENT_NOT_FOUND","ACCOUNT_UNAVAILABLE","CURSOR_UNAVAILABLE","CACHE_UNAVAILABLE","CACHE_NEEDS_VERIFICATION","CACHE_MISS","INTERNAL"]),
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
    schema["properties"]["read_diagnostics"] = read_diagnostics();
    source_properties(&mut schema, "read_source", "live");
    schema
}

fn listing_summary() -> Value {
    let mut schema = all_required(json!({
        "account_id":account_id(),"mailbox":string(),"uid_validity":nullable(uid()),"uid":uid(),
        "subject":nullable(string()),"from":nullable(string()),"date":nullable(string()),"size":nullable(count()),"flags":nullable(array(string())),
        "header_complete":{"type":"boolean","description":"Whether the full message header was observed. With partial coverage, a null subject, from or date is unknown."},
        "classification":{"type":"array","items":enumeration(&["newsletter","automated","unclassified"]),"minItems":1,"maxItems":2,"uniqueItems":true},
        "classification_signals":{"type":"array","items":all_required(json!({
            "field":enumeration(&["list-id","list-unsubscribe","list-unsubscribe-post","auto-submitted","precedence"]),
            "category":enumeration(&["newsletter","automated"])
        })),"maxItems":6,"uniqueItems":true},
        "classification_rule_version":{"const":"headers-v1"},
        "classification_status":enumeration(&["complete","partial"]),"unsubscribe_available":boolean()
    }));
    let properties = schema["properties"]
        .as_object_mut()
        .expect("summary properties");
    properties.insert("preview".into(), json!({"type":"string","maxLength":200}));
    properties.insert("preview_truncated".into(), boolean());
    properties.insert("preview_error".into(), reference("error"));
    properties.insert("cache_evidence".into(), all_required(json!({
        "header_complete":boolean(),"flags_checked_at_ms":nullable(count()),"internal_date":nullable(string()),
        "internal_date_checked_at_ms":nullable(count()),"size_checked_at_ms":nullable(count())
    })));
    properties.insert(
        "cache_match".into(),
        all_required(json!({"metadata":boolean(),"body":boolean()})),
    );
    properties.insert(
        "preview_status".into(),
        enumeration(&[
            "ok",
            "empty",
            "error",
            "unprocessed",
            "budget_skipped",
            "wire_budget_skipped",
        ]),
    );
    schema["dependentRequired"] = json!({
        "preview":["preview_status","preview_truncated"],
        "preview_truncated":["preview_status","preview"],
        "preview_error":["preview_status"],
        "cache_match":["cache_evidence"]
    });
    schema["if"] = json!({"not":{"required":["cache_evidence"]}});
    schema["then"] = json!({"properties":{"flags":array(string())}});
    schema["dependentSchemas"] = json!({"preview_status":{"oneOf":[
        {"properties":{"preview_status":{"const":"ok"}},"required":["preview","preview_truncated"],"not":{"required":["preview_error"]}},
        {"properties":{"preview_status":{"const":"empty"},"preview":{"const":""},"preview_truncated":{"const":false}},"required":["preview","preview_truncated"],"not":{"required":["preview_error"]}},
        {"properties":{"preview_status":{"const":"error"}},"required":["preview_error"],"not":{"required":["preview"]}},
        {"properties":{"preview_status":enumeration(&["unprocessed","budget_skipped","wire_budget_skipped"])},"not":{"anyOf":[{"required":["preview"]},{"required":["preview_error"]}]}}
    ]}});
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
        "unavailable_reason":nullable(enumeration(&["unsupported_transfer_encoding","decoded_attachment_bytes","connection_bytes","cache_miss","cache_omitted"])),
        "delivery":delivery(),"checksum":nullable(checksum())
    }).as_object().unwrap().clone());
    let mut schema = all_required(properties);
    schema["properties"]["cache_state"] = enumeration(&[
        "materialized",
        "derivable_from_raw",
        "missing",
        "omitted",
        "unsupported",
    ]);
    schema
}
fn read_diagnostics() -> Value {
    all_required(json!({
        "source":enumeration(&["bodystructure","raw","cache"]),
        "fallback_reason":nullable(enumeration(&["structure_limit","structure_rejected","mapping_unverified","metadata_conflict","whole_multipart_attachment","header_budget","charset_utf7","body_eof_mismatch"])),
        "limit_kind":nullable(enumeration(&["mime_structure"]))
    }))
}

fn accounts() -> Value {
    let capabilities = all_required(json!({
        "attachment_delivery_modes":{"const":["inline_base64","authenticated_http"]},"part_fetch":{"const":true},
        "cross_mailbox_conversation":{"const":true},"cache_modes":{"enum":[["off"],["off","metadata"],["off","metadata","full"]]}
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
        "conversation_mailboxes",
        "conversation_candidate_headers",
        "conversation_result_locations",
        "conversation_display_body_bytes",
        "conversation_fingerprint_raw_bytes",
        "conversation_search_windows",
        "conversation_search_window_uids",
        "conversation_search_response_bytes",
        "conversation_ordinary_connections",
        "conversation_timeout_seconds",
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
    let mut schema = match name {
        "list_accounts" => accounts(),
        "get_cache_status" => {
            let mut folders = all_required(json!({
                "mailbox":string(),"uid_validity":nullable(uid()),"high_uid":nullable(count()),
                "checked_at_ms":nullable(count()),"dirty":boolean(),"messages":count(),
                "complete_headers":count(),"indexed_headers":count(),"verified_parts":count()
            }));
            folders["properties"]["layers"] = layer_coverage();
            let storage = all_required(json!({
                "max_bytes":count(),"used_bytes":count(),"global_max_bytes":count(),
                "startup_unclean":boolean(),"folders":{"type":"array","maxItems":64,"items":folders}
            }));
            let mut schema = all_required(json!({
                "mode":enumeration(&["off","metadata","full"]),
                "status":enumeration(&["off","ready","unavailable"]),
                "storage":nullable(storage),
                "write_health":all_required(json!({
                    "failed_writes":count(),
                    "last_error":nullable(enumeration(&["quota","queue_full","storage"])),
                    "quota_blocked":boolean()
                }))
            }));
            schema["properties"]["content_pending"] = count();
            schema["properties"]["sync"] = sync_status();
            schema
        }
        "list_mailboxes" => all_required(
            json!({"mailboxes":{"type":"array","maxItems":200,"items":all_required(json!({
                "name":string(),"attributes":string(),"attribute_flags":array(string()),"delimiter":nullable(string()),
                "selectable":boolean(),"special_use":roles(),"role_source":nullable(enumeration(&["server","configuration","protocol"])),"role_candidates":roles()
            }))}}),
        ),
        "list_emails" | "search_emails" => object(
            json!({
                "emails":{"type":"array","maxItems":50,"items":listing_summary()},
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
        "mark_email" | "move_email" | "delete_email" => json!({"oneOf":[
            all_required(json!({"status":{"const":"updated"}})),
            all_required(json!({"status":{"const":"accepted"},"retry_allowed":{"const":false}}))
        ]}),
        "mark_emails" | "move_emails" | "delete_emails" => object(
            json!({
                "items":{"type":"array","minItems":1,"maxItems":50,"items":{"oneOf":[
                    all_required(json!({"uid":uid(),"status":enumeration(&["updated","not_found"])})),
                    all_required(json!({"uid":uid(),"status":enumeration(&["accepted","unprocessed"]),"retry_allowed":{"const":false}})),
                    all_required(json!({"uid":uid(),"status":{"const":"unknown"},"error":reference("error"),"retry_allowed":{"const":false}}))
                ]}},
                "requested":{"type":"integer","minimum":1,"maximum":50},"retry_allowed":{"const":false},
                "error":reference("error")
            }),
            &["items", "requested", "retry_allowed"],
        ),
        "read_conversation" => {
            let mut legacy = all_required(json!({
            "mailbox":string(),"uid_validity":uid(),"messages":{"type":"array","maxItems":20,"items":reference("conversation_message")},"omitted_uids":array(uid()),
            "threading":{"const":"Message-ID/References/In-Reply-To exact identifier graph; no subject matching"},
            "scope":all_required(json!({"start_uid":uid(),"end_uid":count(),"candidates_checked":count(),"seed_uid":uid()})),
            "next_before_uid":nullable(uid()),"candidate_limit_reached":boolean(),"messages_omitted":boolean(),"response_budget_omitted":boolean(),
            "older_uids_omitted":boolean(),"total_body_byte_limit":count(),"skipped_candidates":array(reference("uid_error")),"message_errors":array(reference("uid_error")),"complete":boolean(),"content_trust":trust
            }));
            source_properties(&mut legacy, "read_source", "live");
            return json!({"oneOf":[account_context(legacy,true),cross_conversation()]});
        }
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
        _ => unreachable!("tool has no output schema"),
    };
    if let Some((field, default)) = crate::mail::read_source::source_field(name)
        && schema.get("$ref").is_none()
    {
        source_properties(&mut schema, field, default);
    }
    if name == "search_emails" {
        schema["properties"]["next_cache_cursor"] =
            json!({"type":"string","minLength":51,"maxLength":51,"pattern":"^[A-Za-z0-9_-]+$"});
        schema["dependentRequired"]["next_cache_cursor"] = json!(["cache", "search_source"]);
    }
    if super::is_account_read(name) {
        account_context(
            schema,
            !matches!(
                name,
                "list_mailboxes" | "locate_message" | "diagnose_connectivity" | "get_cache_status"
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
