//! Integration tests for `orchestration_engine::intent_resolver`.
//!
//! Moved out of `src/intent_resolver.rs` so the main source stays test-free.

use macro_engine::SemanticIntent;
use orchestration_engine::intent_resolver::*;
use playbook_store::PlaybookSummary;

fn saved(id: &str, name: &str, portal_url: &str) -> PlaybookSummary {
    PlaybookSummary {
        id: id.into(),
        name: name.into(),
        portal_url: portal_url.into(),
        step_count: 1,
        updated_at: String::new(),
        description: None,
        prompt_key: None,
    }
}

/// The same summary plus a learned prompt key.
fn learned(id: &str, name: &str, portal_url: &str, key: &str) -> PlaybookSummary {
    PlaybookSummary {
        prompt_key: Some(key.into()),
        ..saved(id, name, portal_url)
    }
}

fn portal() -> Result<url::Url, url::ParseError> {
    url::Url::parse("https://github.com/")
}

#[test]
fn name_overlap_beats_host_only_match() {
    let saved = vec![
        saved("1", "reports", "https://github.com/"),
        saved("2", "github-reports", "https://github.com/"),
    ];
    assert_eq!(
        resolve_command("download my latest github report", None, &saved),
        Some(CommandMatch::Saved { id: "2".into() })
    );
}

#[test]
fn host_tokens_route_portal_commands() {
    let saved = vec![saved("1", "reports", "https://github.com/")];
    assert_eq!(
        resolve_command("show github usage", None, &saved),
        Some(CommandMatch::Saved { id: "1".into() })
    );
}

#[test]
fn noise_never_matches() -> Result<(), url::ParseError> {
    let portal = portal()?;
    let saved = vec![saved("1", "reports", "https://portal.example.com/")];
    // Bare noise or pure stopwords: nothing to route on.
    assert_eq!(resolve_command("the", None, &saved), None);
    assert_eq!(resolve_command("", Some(&portal), &saved), None);
    // "com" alone must not match every portal on the internet.
    assert_eq!(
        resolve_command("com", Some(&portal), &saved),
        Some(CommandMatch::Ephemeral {
            intent: SemanticIntent {
                role: "link".into(),
                label_query: "com".into(),
                container_query: None,
                raw_prompt: "com".into(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: Some("com".into()),
            },
        })
    );
    Ok(())
}

#[test]
fn identifiers_skip_static_replays_for_dynamic_resolution() -> Result<(), url::ParseError> {
    let portal = portal()?;
    let saved = vec![saved("1", "github-reports", "https://github.com/")];
    // Token overlap alone would replay — but the ID forces the dynamic
    // path, since the recording cannot honor a specific identifier.
    assert_eq!(
        resolve_command("download github report 0LWQXDWW", Some(&portal), &saved),
        Some(CommandMatch::Ephemeral {
            intent: SemanticIntent {
                role: "link".into(),
                label_query: "report".into(),
                container_query: Some("0LWQXDWW".into()),
                raw_prompt: "download github report 0LWQXDWW".into(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: Some("report".into()),
            },
        })
    );
    // The same prompt without an identifier still replays the recording.
    assert_eq!(
        resolve_command("download github report", Some(&portal), &saved),
        Some(CommandMatch::Saved { id: "1".into() })
    );
    Ok(())
}

#[test]
fn ephemeral_intents_infer_role_and_label() -> Result<(), url::ParseError> {
    let portal = portal()?;
    let cases = [
        ("fill expense report", "textbox", "report", "report"),
        ("click pay now", "button", "pay", "pay"),
        ("open dashboard", "link", "dashboard", "dashboard"),
    ];
    for (prompt, role, label, noun) in cases {
        assert_eq!(
            resolve_command(prompt, Some(&portal), &[]),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: role.into(),
                    label_query: label.into(),
                    container_query: None,
                    raw_prompt: prompt.into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some(noun.into()),
                },
            }),
            "{prompt}"
        );
    }
    // No connected portal, no saved match: honestly nothing.
    assert_eq!(resolve_command("open dashboard", None, &[]), None);
    Ok(())
}

#[test]
fn extract_identifier_reads_shapes_not_sites() {
    // Mixed letter-digit runs of length four or more.
    assert_eq!(
        extract_identifier("download report for ID 0LWQXDWW"),
        Some("0LWQXDWW".into())
    );
    assert_eq!(
        extract_identifier("open 1TUEWZUA now"),
        Some("1TUEWZUA".into())
    );
    assert_eq!(
        extract_identifier("pay INV-2024-001 today"),
        Some("INV-2024-001".into())
    );
    // Structured all-numeric runs: dates and amounts.
    assert_eq!(
        extract_identifier("statements from 2026-09-15"),
        Some("2026-09-15".into())
    );
    assert_eq!(
        extract_identifier("total $1,234.56 due"),
        Some("1,234.56".into())
    );
    // Weak evidence stays out: plain words, short numbers, bare years,
    // hostnames, and single characters.
    for prompt in [
        "download the monthly report",
        "toggle dark mode",
        "pay 42 now",
        "archive 2026 filings",
        "open github.com",
        "a",
    ] {
        assert_eq!(extract_identifier(prompt), None, "{prompt}");
    }
}

#[test]
fn extract_identifier_strips_id_labels() -> Result<(), url::ParseError> {
    // Separator-joined labels yield pure target tokens.
    assert_eq!(
        extract_identifier("download invoice for ID:0LWQXDWW"),
        Some("0LWQXDWW".into())
    );
    assert_eq!(
        extract_identifier("open id-1TUEWZUA now"),
        Some("1TUEWZUA".into())
    );
    // Space-separated labels already split into runs; hyphenated codes
    // and hyphenated prose keep working untouched.
    assert_eq!(
        extract_identifier("pay INV-2024-001 today"),
        Some("INV-2024-001".into())
    );
    // Weak remainders keep scanning instead of returning fragments.
    assert_eq!(extract_identifier("tag ID-42 here"), None);
    // End to end: the scoped container carries the pure token.
    let portal = portal()?;
    assert_eq!(
        resolve_command("download invoice for ID:0LWQXDWW", Some(&portal), &[]),
        Some(CommandMatch::Ephemeral {
            intent: SemanticIntent {
                role: "link".into(),
                label_query: "invoice".into(),
                container_query: Some("0LWQXDWW".into()),
                raw_prompt: "download invoice for ID:0LWQXDWW".into(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: Some("invoice".into()),
            },
        })
    );
    Ok(())
}

#[test]
fn variable_extractor_detects_ids_and_amounts_cleanly() {
    // Grounded scenario: the ID appears in both prompt and container but
    // templates exactly once; the schema carries type and UI metadata.
    let extraction = extract_dynamic_variables(
        "download invoice for 1TUEWZUA",
        "Download invoice",
        "Invoice 1TUEWZUA",
    );
    assert_eq!(extraction.template, "download invoice for {{invoice_id}}");
    assert_eq!(extraction.variables.len(), 1);
    let variable = &extraction.variables[0];
    assert_eq!(variable.name, "invoice_id");
    assert_eq!(variable.kind, VariableKind::Id);
    assert_eq!(variable.default_value, "1TUEWZUA");
    assert_eq!(variable.field_type, "text");
    assert_eq!(variable.ui_label, "Invoice ID");
    // Amounts, dates, and quoted entities classify distinctly.
    let extraction =
        extract_dynamic_variables("pay $45.00 on Sep 17 for \"subheader.lol\"", "", "");
    assert_eq!(
        extraction.template,
        "pay {{amount}} on {{date}} for {{entity_name}}"
    );
    let kinds: Vec<VariableKind> = extraction
        .variables
        .iter()
        .map(|variable| variable.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            VariableKind::Amount,
            VariableKind::Date,
            VariableKind::Entity
        ]
    );
    // Plain prompts stay literal with an empty schema.
    let extraction = extract_dynamic_variables("toggle dark mode", "", "");
    assert_eq!(extraction.template, "toggle dark mode");
    assert!(extraction.variables.is_empty());
}

#[test]
fn identifiers_scope_ephemeral_intents() -> Result<(), url::ParseError> {
    let portal = portal()?;
    // The ID leaves the label stream and becomes the container scope.
    assert_eq!(
        resolve_command("download report for ID 0LWQXDWW", Some(&portal), &[]),
        Some(CommandMatch::Ephemeral {
            intent: SemanticIntent {
                role: "link".into(),
                label_query: "report".into(),
                container_query: Some("0LWQXDWW".into()),
                raw_prompt: "download report for ID 0LWQXDWW".into(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: Some("report".into()),
            },
        })
    );
    // A lone identifier still runs: it doubles as a last-resort label.
    assert_eq!(
        resolve_command("0LWQXDWW", Some(&portal), &[]),
        Some(CommandMatch::Ephemeral {
            intent: SemanticIntent {
                role: "link".into(),
                label_query: "0LWQXDWW".into(),
                container_query: Some("0LWQXDWW".into()),
                raw_prompt: "0LWQXDWW".into(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: None,
            },
        })
    );
    // Plain prompts keep working with no scope attached.
    assert_eq!(
        resolve_command("toggle dark mode", Some(&portal), &[]),
        Some(CommandMatch::Ephemeral {
            intent: SemanticIntent {
                role: "link".into(),
                label_query: "mode".into(),
                container_query: None,
                raw_prompt: "toggle dark mode".into(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: Some("mode".into()),
            },
        })
    );
    Ok(())
}

#[test]
fn ephemeral_names_always_validate() -> Result<(), Box<dyn std::error::Error>> {
    let portal = portal()?;
    for prompt in ["Pay my !!! reports ???", "", "a"] {
        let name = ephemeral_name(prompt);
        let playbook = playbook_store::Playbook::new(
            name,
            portal.clone(),
            vec![playbook_store::Step::Semantic {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "x".into(),
                    container_query: None,
                    raw_prompt: String::new(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: None,
                },
            }],
        )?;
        assert!(!playbook.name.is_empty());
    }
    assert_eq!(
        ephemeral_name("Pay my monthly reports"),
        "pay-monthly-reports"
    );
    Ok(())
}

#[test]
fn structured_pass_handles_any_case_and_format() -> Result<(), url::ParseError> {
    let portal = portal()?;
    // Lowercase ID: preserved as typed, matching stays case-insensitive
    // downstream in the executor.
    let Some(parsed) = parse_intent_structured("download invoice for me for ID 0lwqxdww") else {
        panic!("lowercase ID parses");
    };
    assert_eq!(parsed.container_query.as_deref(), Some("0lwqxdww"));
    assert!(!parsed.label_query.is_empty());
    // Attributes plus date collapse into one container scope.
    let Some(parsed) = parse_intent_structured("download the $4 declined invoice from June 12")
    else {
        panic!("attributes parse");
    };
    let Some(scope) = parsed.container_query else {
        panic!("scope present");
    };
    assert!(scope.contains("$4"), "{scope}");
    assert!(scope.to_ascii_lowercase().contains("declined"), "{scope}");
    assert!(scope.to_ascii_lowercase().contains("june"), "{scope}");
    assert_eq!(parsed.label_query, "invoice");
    // Relative position and relative time never become labels or vetoes.
    assert_eq!(
        parse_intent_structured("download the second invoice in the list")
            .map(|parsed| parsed.label_query),
        Some("invoice".into())
    );
    assert_eq!(
        parse_intent_structured("get the receipt for last week").map(|parsed| parsed.label_query),
        Some("receipt".into())
    );
    // End-to-end: lowercase prompt still scopes the ephemeral intent.
    assert_eq!(
        resolve_command("download invoice for ID 0lwqxdww", Some(&portal), &[]),
        Some(CommandMatch::Ephemeral {
            intent: SemanticIntent {
                role: "link".into(),
                label_query: "invoice".into(),
                container_query: Some("0lwqxdww".into()),
                raw_prompt: "download invoice for ID 0lwqxdww".into(),
                ordinal_index: None,
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: Some("invoice".into()),
            },
        })
    );
    Ok(())
}

#[test]
fn navigation_verbs_isolate_target_labels() -> Result<(), url::ParseError> {
    // Verb isolation lives in the deterministic fallback (there is no
    // LLM system prompt in-tree: the provider speaks the JSON contract
    // and the fallback must already separate verbs from targets, since
    // it runs whenever the provider is unset). Navigation verbs never
    // leak into the label; the role carries the click/navigate action
    // (`link` here — `SemanticIntent` has no separate action field, and
    // labels normalize to lowercase for case-insensitive matching).
    let portal = portal()?;
    for (prompt, label, noun) in [
        ("open the Analytics", "analytics", "analytic"),
        ("open the Deployments", "deployments", "deployment"),
    ] {
        assert_eq!(
            resolve_command(prompt, Some(&portal), &[]),
            Some(CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: label.into(),
                    container_query: None,
                    raw_prompt: prompt.into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some(noun.into()),
                },
            }),
            "{prompt}"
        );
    }
    Ok(())
}

#[test]
fn ordinals_parse_to_positions_without_polluting_labels() -> Result<(), url::ParseError> {
    // Rank words reuse the existing ordinal vocabulary; a bare `last`
    // beside time words stays temporal (no position).
    assert_eq!(
        parse_ordinal("download the second invoice"),
        (Some(1), false)
    );
    assert_eq!(parse_ordinal("click the 2nd button"), (Some(1), false));
    assert_eq!(parse_ordinal("open the first report"), (Some(0), false));
    assert_eq!(parse_ordinal("open the last invoice"), (None, true));
    assert_eq!(
        parse_ordinal("get the receipt for last week"),
        (None, false)
    );
    assert_eq!(parse_ordinal("toggle dark mode"), (None, false));
    // End to end: position rides the ephemeral intent, label stays clean.
    let portal = portal()?;
    assert_eq!(
        resolve_command("download the second invoice", Some(&portal), &[]),
        Some(CommandMatch::Ephemeral {
            intent: SemanticIntent {
                role: "link".into(),
                label_query: "invoice".into(),
                container_query: None,
                raw_prompt: "download the second invoice".into(),
                ordinal_index: Some(1),
                is_last: false,
                is_plural: false,
                entry_url: None,
                primary_target_noun: Some("invoice".into()),
            },
        })
    );
    Ok(())
}

#[test]
fn composite_prompt_decomposes_into_sequential_steps() -> Result<(), url::ParseError> {
    let portal = portal()?;
    // Two connectors, two ordered ephemeral intents; each segment keeps
    // its own label, raw prompt, and position data.
    let steps = decompose_command("open settings and turn off dark mode", Some(&portal), &[]);
    assert_eq!(
        steps,
        vec![
            CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "settings".into(),
                    container_query: None,
                    raw_prompt: "open settings".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some("setting".into()),
                },
            },
            CommandMatch::Ephemeral {
                intent: SemanticIntent {
                    role: "link".into(),
                    label_query: "mode".into(),
                    container_query: None,
                    raw_prompt: "turn off dark mode".into(),
                    ordinal_index: None,
                    is_last: false,
                    is_plural: false,
                    entry_url: None,
                    primary_target_noun: Some("mode".into()),
                },
            },
        ]
    );
    // Comma-then splits without leaving punctuation on either side, and
    // single-action prompts stay single-element.
    let steps = decompose_command("open settings, then turn off dark mode", Some(&portal), &[]);
    assert_eq!(steps.len(), 2);
    // Case-insensitive connectors split identically; only the pass-through
    // raw text keeps its original casing.
    let loud = decompose_command("OPEN SETTINGS AND TURN OFF DARK MODE", Some(&portal), &[]);
    let shape = |steps: &[CommandMatch]| -> Vec<String> {
        steps
            .iter()
            .map(|step| match step {
                CommandMatch::Saved { id } => format!("saved:{id}"),
                CommandMatch::Ephemeral { intent } => {
                    format!("{}:{}", intent.role, intent.label_query)
                }
            })
            .collect()
    };
    assert_eq!(shape(&steps), shape(&loud));
    assert_eq!(
        decompose_command("toggle dark mode", Some(&portal), &[]).len(),
        1
    );
    assert!(decompose_command("", Some(&portal), &[]).is_empty());
    Ok(())
}

#[test]
fn plural_intent_extracted_on_all_keyword() -> Result<(), url::ParseError> {
    // Collection markers ride the intent without polluting the label:
    // `all`/`every`/`each` set the flag, everything else stays singular.
    assert!(parse_plural("download all invoices"));
    assert!(parse_plural("get every receipt"));
    assert!(parse_plural("open each statement"));
    assert!(!parse_plural("download invoice"));
    assert!(!parse_plural("toggle dark mode"));
    assert!(!parse_plural("overall summary"));
    let portal = portal()?;
    assert_eq!(
        resolve_command("download all invoices", Some(&portal), &[]),
        Some(CommandMatch::Ephemeral {
            intent: SemanticIntent {
                role: "link".into(),
                label_query: "invoices".into(),
                container_query: None,
                raw_prompt: "download all invoices".into(),
                ordinal_index: None,
                is_last: false,
                is_plural: true,
                entry_url: None,
                primary_target_noun: Some("invoice".into()),
            },
        })
    );
    Ok(())
}

#[test]
fn plural_prompts_skip_single_step_saved_replays() -> Result<(), url::ParseError> {
    let portal = portal()?;
    let saved = vec![saved("1", "download-invoices", "https://github.com/")];
    // Token overlap alone would replay — but plurality forces the dynamic
    // batch path, since a one-click recording cannot honor "all".
    let matched = resolve_command(
        "download all my invoices from github",
        Some(&portal),
        &saved,
    );
    let Some(CommandMatch::Ephemeral { intent }) = matched else {
        panic!("plural prompt bypasses the saved replay");
    };
    assert!(intent.is_plural);
    // The singular twin still replays the recording untouched.
    assert_eq!(
        resolve_command("download invoice from github", Some(&portal), &saved),
        Some(CommandMatch::Saved { id: "1".into() })
    );
    Ok(())
}

#[test]
fn test_grammar_parsing_without_portal_whitelist() -> Result<(), url::ParseError> {
    // Prepositional complement: the noun before the cue is the artifact
    // the batch acts on, the token after it names the domain.
    let parsed = parse_grammar("download all my invoices from github", None);
    assert_eq!(parsed.artifact_noun.as_deref(), Some("invoice"));
    assert_eq!(parsed.site_context.as_deref(), Some("github"));
    assert_eq!(parsed.target_noun, None);
    let parsed = parse_grammar("download reports from linear", None);
    assert_eq!(parsed.artifact_noun.as_deref(), Some("report"));
    assert_eq!(parsed.site_context.as_deref(), Some("linear"));
    assert_eq!(parsed.target_noun, None);
    // Direct action: no complement, so the direct object *is* the target.
    let parsed = parse_grammar("open amazon for me", None);
    assert_eq!(parsed.target_noun.as_deref(), Some("amazon"));
    assert_eq!(parsed.site_context, None);
    assert_eq!(parsed.artifact_noun, None);
    // Structure decides, not vocabulary: a site nobody enumerated parses
    // exactly like `github`, and `on`/`at` read the same as `from`.
    for (prompt, artifact, site) in [
        ("download invoices from acmecorp", "invoice", "acmecorp"),
        ("grab my statements on zzyzxbank", "statement", "zzyzxbank"),
        ("export receipts at quuxvendor", "receipt", "quuxvendor"),
    ] {
        let parsed = parse_grammar(prompt, None);
        assert_eq!(parsed.artifact_noun.as_deref(), Some(artifact), "{prompt}");
        assert_eq!(parsed.site_context.as_deref(), Some(site), "{prompt}");
    }
    // A cue with no noun before it is a phrasal-verb particle, not a
    // complement: `click on pay now` still targets the control.
    let parsed = parse_grammar("click on pay now", None);
    assert_eq!(parsed.site_context, None);
    assert_eq!(parsed.target_noun.as_deref(), Some("pay"));
    // Articles between the cue and the site don't block it, and `in`
    // reads as a site cue: "open profile for me in the reddit" names
    // reddit, not a bare "profile" search.
    for (prompt, artifact, site) in [
        ("open profile for me in the reddit", "profile", "reddit"),
        ("download invoices from the acmecorp", "invoice", "acmecorp"),
        ("grab my statements in zzyzxbank", "statement", "zzyzxbank"),
    ] {
        let parsed = parse_grammar(prompt, None);
        assert_eq!(parsed.artifact_noun.as_deref(), Some(artifact), "{prompt}");
        assert_eq!(parsed.site_context.as_deref(), Some(site), "{prompt}");
    }
    // A cue followed by structure rather than a place is not a complement
    // either: months, ordinals, and digits never name a destination.
    let parsed = parse_grammar("download the declined invoice from June 12", None);
    assert_eq!(parsed.site_context, None);
    assert_eq!(parsed.target_noun.as_deref(), Some("invoice"));
    // Parsing is connection-independent: the same prompt yields the same
    // slots whether parked on the named portal, on another one, or on
    // nothing at all.
    let portal = portal()?;
    let google = url::Url::parse("https://google.com")?;
    for origin in [None, Some(&portal), Some(&google)] {
        let parsed = parse_grammar("download all my invoices from github", origin);
        assert_eq!(parsed.artifact_noun.as_deref(), Some("invoice"));
        assert_eq!(parsed.site_context.as_deref(), Some("github"));
    }
    // Nothing content-bearing leaves every slot empty.
    assert_eq!(parse_grammar("", None), ParsedGrammar::default());
    assert_eq!(parse_grammar("the", None), ParsedGrammar::default());
    Ok(())
}

#[test]
fn confidence_gates_on_structure_not_slot_count() -> Result<(), url::ParseError> {
    // Plain imperatives with complete slots are trusted outright.
    for prompt in [
        "download invoices from github",
        "download all my invoices from github",
        "grab my statements on zzyzxbank",
        "export receipts at quuxvendor",
        // Direct action: the object is the destination, and that is a
        // complete parse for its pattern.
        "open amazon for me",
        "click pay now",
        "toggle dark mode",
        "fill expense report",
    ] {
        assert_eq!(
            parse_grammar(prompt, None).confidence,
            Confidence::High,
            "{prompt} is a plain imperative"
        );
    }
    // A subordinate clause means more grammar than this parser models.
    // Note both slots *are* populated here — slot count alone would have
    // called this confident, and the artifact would have been `owe`.
    let parsed = parse_grammar("pull up what I owe on aws", None);
    assert_eq!(parsed.confidence, Confidence::Low);
    assert_eq!(parsed.artifact_noun.as_deref(), Some("owe"));
    assert_eq!(parsed.site_context.as_deref(), Some("aws"));
    for prompt in [
        "pull up what I owe on aws",
        "show me which invoices are overdue",
        "find whatever bills are on github",
        "get the thing that I paid for",
    ] {
        assert_eq!(
            parse_grammar(prompt, None).confidence,
            Confidence::Low,
            "{prompt} carries a subordinate clause"
        );
    }
    // No verb to anchor on: a bare noun phrase is not a command.
    for prompt in ["com", "statements", "my invoices", "the github thing"] {
        assert_eq!(
            parse_grammar(prompt, None).confidence,
            Confidence::Low,
            "{prompt} has no verb head"
        );
    }
    // Nothing content-bearing is never confident, and the default agrees.
    assert_eq!(parse_grammar("", None).confidence, Confidence::Low);
    assert_eq!(Confidence::default(), Confidence::Low);
    assert!(Confidence::High.is_high());
    assert!(!Confidence::Low.is_high());
    // Confidence is a property of the prompt, not of the live session.
    let portal = portal()?;
    assert_eq!(
        parse_grammar("download invoices from github", Some(&portal)).confidence,
        Confidence::High
    );
    Ok(())
}

#[test]
fn role_inference_only_emits_the_shared_action_vocabulary() {
    // The parser seam validates model output against `INTENT_ROLES`, so
    // that list must stay exactly what `role_for` can produce — otherwise
    // the two vocabularies drift and a valid parse gets rejected (or an
    // invalid one accepted).
    for prompt in [
        "fill expense report",
        "type my address",
        "enter the code",
        "click pay now",
        "press submit",
        "submit the form",
        "tap continue",
        "select a plan",
        "choose the date",
        "download invoices from github",
        "open amazon",
        "",
    ] {
        let role = role_for(&content_tokens(prompt));
        assert!(INTENT_ROLES.contains(&role), "{prompt} inferred {role}");
    }
}

#[test]
fn prompt_keys_normalize_phrasing_and_refuse_unusable_keys() {
    // Casing and spacing collapse, so one phrasing is one key.
    assert_eq!(
        prompt_key("  Pull up   what I owe ON aws ").as_deref(),
        Some("pull up what i owe on aws")
    );
    assert_eq!(
        prompt_key("download github invoices").as_deref(),
        Some("download github invoices")
    );
    // Deliberately not stemmed or stopword-filtered: a key identifies the
    // exact phrasing the user saved, so distinct prompts stay distinct.
    assert_ne!(
        prompt_key("download my invoices"),
        prompt_key("download invoices")
    );
    // Unusable keys are refused rather than stored: blanks would match
    // every blank prompt, and truncating an oversized prompt would let
    // two different commands collide on one workflow.
    assert_eq!(prompt_key(""), None);
    assert_eq!(prompt_key("   \n\t "), None);
    let oversized = "a ".repeat(playbook_store::schema::MAX_PROMPT_KEY_LEN);
    assert_eq!(prompt_key(&oversized), None);
}

#[test]
fn learned_prompt_keys_win_routing_without_bypassing_guards() -> Result<(), url::ParseError> {
    let portal = portal()?;
    // An exact key outranks a stronger token-overlap competitor: the
    // phrasing was taught, so it is not a guess to be outvoted.
    let with_key = vec![
        saved("1", "github-invoices", "https://github.com/"),
        learned(
            "2",
            "aws-bills",
            "https://aws.amazon.com/",
            "get my github bills",
        ),
    ];
    assert_eq!(
        resolve_command("get my github bills", Some(&portal), &with_key),
        Some(CommandMatch::Saved { id: "2".into() }),
        "the learned key wins over name overlap"
    );
    // Without the key, the same prompt routes by overlap as before, so
    // the key is additive rather than a behavior change.
    let unlearned = vec![
        saved("1", "github-invoices", "https://github.com/"),
        saved("2", "aws-bills", "https://aws.amazon.com/"),
    ];
    assert_eq!(
        resolve_command("get my github bills", Some(&portal), &unlearned),
        Some(CommandMatch::Saved { id: "1".into() })
    );
    // The key ranks candidates; it never overrides the guards that keep
    // replay honest. Saved replay resolves one control per step, so a
    // plural prompt still takes the dynamic path even with its own key
    // stored — replaying it would click once and under-deliver.
    let plural = vec![learned(
        "9",
        "all-invoices",
        "https://github.com/",
        "download all my invoices from github",
    )];
    assert!(
        matches!(
            resolve_command(
                "download all my invoices from github",
                Some(&portal),
                &plural
            ),
            Some(CommandMatch::Ephemeral { .. })
        ),
        "plurality still forces the dynamic batch lane"
    );
    // Same for a prompt carrying a specific scope: the recording has no
    // parameter slot for `0LWQXDWW`.
    let scoped = vec![learned(
        "9",
        "one-invoice",
        "https://github.com/",
        "download invoice 0lwqxdww",
    )];
    assert!(matches!(
        resolve_command("download invoice 0LWQXDWW", Some(&portal), &scoped),
        Some(CommandMatch::Ephemeral { .. })
    ));
    Ok(())
}

#[test]
fn primary_noun_strips_modifiers_and_singularizes() -> Result<(), url::ParseError> {
    // Modifiers never survive: the anchor is always the content word in
    // stem form. Lone identifiers carry no anchor at all.
    assert_eq!(
        extract_primary_noun("download all my invoices", None).as_deref(),
        Some("invoice")
    );
    assert_eq!(
        extract_primary_noun("open the Analytics", None).as_deref(),
        Some("analytic")
    );
    // Trailing site words yield to the object noun: the grammar reads
    // them as the destination complement, not the batch target.
    let portal = portal()?;
    assert_eq!(
        extract_primary_noun("download all my invoices from github", Some(&portal)).as_deref(),
        Some("invoice")
    );
    // Cross-portal starts hold too: parked on `google.com`, the `github`
    // trailer is still the destination slot, so the anchor stays the
    // content noun and pre-navigation can fire.
    let google = url::Url::parse("https://google.com")?;
    assert_eq!(
        extract_primary_noun("download all my invoices from github", Some(&google)).as_deref(),
        Some("invoice")
    );
    // Identifier-stripped streams only: a lone identifier arrives
    // empty after cleaning, exactly as `resolve_command` passes it.
    assert_eq!(extract_primary_noun("", None), None);
    assert_eq!(extract_primary_noun("the", None), None);
    // End to end: the ephemeral intent carries the anchor.
    assert_eq!(
        resolve_command("download all my invoices", Some(&portal), &[]),
        Some(CommandMatch::Ephemeral {
            intent: SemanticIntent {
                role: "link".into(),
                label_query: "invoices".into(),
                container_query: None,
                raw_prompt: "download all my invoices".into(),
                ordinal_index: None,
                is_last: false,
                is_plural: true,
                entry_url: None,
                primary_target_noun: Some("invoice".into()),
            },
        })
    );
    Ok(())
}

#[test]
fn scoped_attributes_skip_saved_replays() -> Result<(), url::ParseError> {
    let portal = portal()?;
    let saved = vec![saved("1", "invoices", "https://github.com/")];
    // Name overlap alone would replay — but amount/status/date scope
    // forces the dynamic path, since the recording has no parameter slots.
    let matched = resolve_command(
        "download the $4 declined invoice from June 12",
        Some(&portal),
        &saved,
    );
    assert!(matches!(matched, Some(CommandMatch::Ephemeral { .. })));
    Ok(())
}

#[test]
fn tier_zero_matches_lifecycle_commands_through_filler() {
    // Every phrasing of "start the browser" resolves without a search —
    // the frozen normalizer strips politeness affixes from the ends and
    // matches the closed phrase set exactly, so "new blank page" works
    // (the full stopword filter would eat "new").
    for prompt in [
        "spin up browser for me",
        "spin up the browser",
        "please spin up browser",
        "spin up browser for us",
        "spin up browser, thanks",
        "spin up browser thank you",
        "open the browser",
        "launch browser",
        "show me the browser",
        "open a blank tab",
        "open a blank tab for me",
        "new blank page",
    ] {
        assert_eq!(
            resolve_app_command(prompt),
            Some(AppCommand::OpenBlankBrowser),
            "{prompt}"
        );
    }
}

#[test]
fn tier_zero_rejects_near_misses() {
    // Closed-world: an extra content token, or no lifecycle verb, falls
    // through instead of being swallowed.
    for prompt in [
        "open amazon for me",
        "open browser settings",
        "download the browser",
        "browser",
        "spin up",
        "",
        "open amazon and flipkart",
    ] {
        assert_eq!(resolve_app_command(prompt), None, "{prompt}");
    }
}

#[test]
fn direct_open_covers_open_phrasings_and_filler() {
    // High-confidence single-target opens, with and without filler.
    for prompt in [
        "open amazon",
        "open amazon for me",
        "please open amazon",
        "open amazon.in",
        "visit github",
        "launch amazon",
        "go to amazon",
    ] {
        let grammar = parse_grammar(prompt, None);
        assert!(grammar.confidence.is_high(), "{prompt}");
        assert!(is_direct_open(prompt, &grammar), "{prompt}");
    }
    // Only "please" may precede the heading verb: "kindly open amazon"
    // is not verb-headed, so it defers to the parser seam rather than
    // taking the fast path.
    let grammar = parse_grammar("kindly open amazon", None);
    assert_eq!(grammar.confidence, Confidence::Low);
    assert!(!is_direct_open("kindly open amazon", &grammar));
}

#[test]
fn direct_open_rejects_artifacts_clauses_and_batches() {
    // Retrieval keeps the search path.
    for prompt in [
        "download all my invoices from github",
        "find amazon",
        "check amazon prices",
    ] {
        let grammar = parse_grammar(prompt, None);
        assert!(!is_direct_open(prompt, &grammar), "{prompt}");
    }
    // Multi-target prompts must re-resolve through batch consent, never
    // silently open one target.
    for prompt in ["open amazon and flipkart", "open amazon or flipkart"] {
        let grammar = parse_grammar(prompt, None);
        assert!(!is_direct_open(prompt, &grammar), "{prompt}");
    }
    // A subordinate clause is not a plain imperative.
    let grammar = parse_grammar("pull up what I owe on aws", None);
    assert!(!is_direct_open("pull up what I owe on aws", &grammar));
}

#[test]
fn direct_open_never_fills_the_site_slot_with_a_verb() {
    // Regression: `tokens` drops one-character tokens, so in
    // `open x for me` the site (`x`) was invisible and the verb
    // survived as the target noun — grounding `open` → `open.com`.
    for (prompt, site) in [
        ("open x for me", "x"),
        ("open the x for me", "x"),
        ("please open x", "x"),
        ("go to x", "x"),
    ] {
        let grammar = parse_grammar(prompt, None);
        assert_eq!(grammar.target_noun.as_deref(), Some(site), "{prompt}");
        assert!(grammar.confidence.is_high(), "{prompt}");
        assert!(is_direct_open(prompt, &grammar), "{prompt}");
    }
    // A lone verb is not a destination: honest miss, never a verb slot.
    for prompt in ["open", "open the", "launch"] {
        let grammar = parse_grammar(prompt, None);
        assert_eq!(grammar.target_noun, None, "{prompt}");
        assert!(!is_direct_open(prompt, &grammar), "{prompt}");
    }
}

#[test]
fn direct_open_keeps_multi_character_targets_unchanged() {
    // The single-character fallback must not disturb the normal path.
    for (prompt, site) in [
        ("open claude for me", "claude"),
        ("open the claude for me", "claude"),
        ("open gemini for me", "gemini"),
        ("open amazon for me", "amazon"),
    ] {
        let grammar = parse_grammar(prompt, None);
        assert_eq!(grammar.target_noun.as_deref(), Some(site), "{prompt}");
        assert!(is_direct_open(prompt, &grammar), "{prompt}");
    }
}
