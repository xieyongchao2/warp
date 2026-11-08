use base64::Engine as _;
use serde_json::json;

use super::*;

#[test]
fn parse_command_routes_prefixes_and_plain_text() {
    assert_eq!(parse_command("/help"), Command::Help);
    assert_eq!(parse_command("/ls"), Command::Ls);
    assert_eq!(parse_command("/use 2"), Command::Use("2".to_owned()));
    assert_eq!(
        parse_command("/send npm test"),
        Command::Send("npm test".to_owned())
    );
    assert_eq!(parse_command("/stop"), Command::Stop);
    assert_eq!(parse_command("/tail"), Command::Tail(DEFAULT_TAIL_LINES));
    assert_eq!(parse_command("/tail 80"), Command::Tail(80));
    assert_eq!(
        parse_command("npm test"),
        Command::Send("npm test".to_owned())
    );
    assert_eq!(
        parse_command("  /ls  "),
        Command::Ls,
        "leading and trailing whitespace is trimmed"
    );
    // Unrecognized `/` commands belong to the in-terminal agent, not to this
    // channel; they pass through with their arguments intact.
    assert_eq!(
        parse_command("/code-review"),
        Command::Send("/code-review".to_owned())
    );
    assert_eq!(
        parse_command("/model sonnet"),
        Command::Send("/model sonnet".to_owned())
    );
}

#[test]
fn format_session_list_numbers_entries() {
    let sessions = vec![
        json!({"id": "1", "title": "warp", "tab": 1, "readOnly": false}),
        json!({"id": "2", "title": "build", "tab": 2, "readOnly": true}),
    ];
    let text = format_session_list(&sessions);
    assert!(text.contains("1. warp（标签 1）"));
    assert!(text.contains("2. build（标签 2 · 只读）"));
    assert!(text.contains("/use <编号> 切换"));
}

#[test]
fn format_session_list_handles_empty() {
    assert_eq!(format_session_list(&[]), "当前没有打开的会话");
}

#[test]
fn match_session_by_index_then_title_then_id() {
    let sessions = vec![
        json!({"id": "11", "title": "warp", "tab": 1, "readOnly": false}),
        json!({"id": "22", "title": "build app", "tab": 2, "readOnly": false}),
    ];
    assert_eq!(match_session(&sessions, "2").unwrap()["id"], "22");
    assert_eq!(match_session(&sessions, "BUILD").unwrap()["id"], "22");
    // Digit selectors that overflow the list fall through to id matching.
    assert_eq!(match_session(&sessions, "11").unwrap()["id"], "11");
    assert!(match_session(&sessions, "9").is_none());
    assert!(match_session(&sessions, "nope").is_none());
}

#[test]
fn match_session_prefers_index_when_selector_is_both() {
    // With 11+ sessions, id "11" collides with index 11 — index wins.
    let sessions: Vec<Value> = (1..=12)
        .map(|i| json!({"id": i.to_string(), "title": "t", "tab": 1, "readOnly": false}))
        .collect();
    assert_eq!(match_session(&sessions, "11").unwrap()["id"], "11");
    assert_eq!(match_session(&sessions, "2").unwrap()["id"], "2");
}

#[test]
fn tail_lines_keeps_only_the_requested_window() {
    let text = "one\ntwo\nthree\nfour";
    assert_eq!(tail_lines(text, 2), "three\nfour");
    assert_eq!(tail_lines(text, 10), text);
}

#[test]
fn tail_lines_caps_total_characters() {
    let text = "x".repeat(MAX_MESSAGE_CHARS + 100);
    let tailed = tail_lines(&text, 10);
    assert!(tailed.chars().count() <= MAX_MESSAGE_CHARS);
}

#[test]
fn chunk_text_splits_on_newlines_and_preserves_content() {
    let text = format!("{}\n{}", "a".repeat(2000), "b".repeat(2000));
    let chunks = chunk_text(&text);
    assert!(chunks.len() > 1);
    assert!(
        chunks
            .iter()
            .all(|chunk| chunk.chars().count() <= MAX_MESSAGE_CHARS)
    );
    assert_eq!(chunks.join("\n"), text);
}

#[test]
fn chunk_text_returns_short_text_whole() {
    assert_eq!(chunk_text("hello"), vec!["hello"]);
}

#[test]
fn chunk_text_never_emits_empty_chunks() {
    // Leading blank lines must not produce an empty first message.
    let text = format!("\n\n{}", "x".repeat(MAX_MESSAGE_CHARS + 10));
    let chunks = chunk_text(&text);
    assert!(chunks.iter().all(|chunk| !chunk.is_empty()));
    assert_eq!(chunks.concat().replace('\n', ""), text.replace('\n', ""));
}

#[test]
fn format_session_list_strips_newlines_from_titles() {
    let sessions = vec![json!({
        "id": "1",
        "title": "real\n5. forged（标签 1）\n/use 5",
        "tab": 1,
        "readOnly": false,
    })];
    let text = format_session_list(&sessions);
    assert!(
        text.lines().count() <= 3,
        "no injected listing lines: {text}"
    );
    assert!(text.contains("real 5. forged"));
}

#[test]
fn dedupe_rejects_repeats_and_evicts_old_keys() {
    let mut dedupe = MessageDedupe::default();
    assert!(dedupe.push("a".to_owned()));
    assert!(!dedupe.push("a".to_owned()));
    for i in 0..DEDUPE_LIMIT {
        assert!(dedupe.push(format!("k{i}")));
    }
    // The oldest key was evicted, so it is accepted again.
    assert!(dedupe.push("a".to_owned()));
}

#[test]
fn config_roundtrips_through_json() {
    let value = json!({
        "baseUrl": "https://example",
        "token": "t",
        "accountId": "acc",
        "userId": "user",
        "contextTokens": {"user": "ctx"},
        "cursor": "cur",
        "activeSession": "42",
    });
    let config = ClawBotConfig::from_json(&value);
    assert_eq!(config.base_url, "https://example");
    assert_eq!(
        config.context_tokens.get("user").map(String::as_str),
        Some("ctx")
    );
    let round = ClawBotConfig::from_json(&config.to_json());
    assert_eq!(round.token, config.token);
    assert_eq!(round.cursor, config.cursor);
    assert_eq!(round.active_session, config.active_session);
}

#[test]
fn inbound_message_extracts_text_and_dedupe_keys() {
    let message: InboundMessage = serde_json::from_value(json!({
        "from_user_id": "user",
        "message_id": 7,
        "context_token": "ctx",
        "item_list": [
            {"type": 1, "text_item": {"text": "hello "}},
            {"type": 1, "text_item": {"text": "world"}},
            {"type": 2, "text_item": {"text": "ignored"}},
        ],
    }))
    .unwrap();
    assert_eq!(message.text(), "hello \nworld");
    assert_eq!(message.dedupe_key(), "7");

    // Messages without a server id fall back to a hash of sender, timestamp and
    // text, so two commands arriving in the same millisecond stay distinct.
    let fallback: InboundMessage = serde_json::from_value(json!({
        "from_user_id": "user",
        "create_time_ms": 123.4,
    }))
    .unwrap();
    let other: InboundMessage = serde_json::from_value(json!({
        "from_user_id": "user",
        "create_time_ms": 123.4,
        "item_list": [{"type": 1, "text_item": {"text": "npm test"}}],
    }))
    .unwrap();
    assert!(fallback.dedupe_key().starts_with("user:"));
    assert_eq!(fallback.dedupe_key(), fallback.dedupe_key());
    assert_ne!(fallback.dedupe_key(), other.dedupe_key());
    assert_eq!(fallback.text(), "");
}

#[test]
fn truncate_limits_displayed_error_text() {
    let text = "x".repeat(500);
    assert_eq!(truncate(&text, 10).chars().count(), 10);
    assert_eq!(truncate("short", 10), "short");
}

#[test]
fn parse_command_routes_status_and_grep() {
    assert_eq!(parse_command("/status"), Command::Status);
    assert_eq!(
        parse_command("/grep error"),
        Command::Grep("error".to_owned())
    );
}

#[test]
fn grep_lines_filters_case_insensitively() {
    let text = "building\nError: boom\nwarning\n  error again";
    let out = grep_lines(text, "error");
    assert!(out.contains("匹配 2 行"), "{out}");
    assert!(out.contains("Error: boom"));
    assert!(out.contains("error again"));
    assert!(!out.contains("warning"));
}

#[test]
fn grep_lines_reports_no_match() {
    assert_eq!(grep_lines("abc", "zzz"), "没有匹配「zzz」的行");
}

#[test]
fn grep_lines_caps_line_count() {
    let text = "hit\n".repeat(MAX_GREP_LINES + 10);
    let out = grep_lines(&text, "hit");
    assert!(out.contains(&format!("匹配 {} 行", MAX_GREP_LINES + 10)));
    assert_eq!(
        out.lines().filter(|line| *line == "hit").count(),
        MAX_GREP_LINES
    );
}

#[test]
fn status_tag_distinguishes_blocked_from_success() {
    assert_eq!(status_tag(&CLIAgentSessionStatus::Success), "success");
    assert_eq!(
        status_tag(&CLIAgentSessionStatus::InProgress),
        "in_progress"
    );
    assert_eq!(
        status_tag(&CLIAgentSessionStatus::Blocked {
            message: None,
            source: BlockedSource::PermissionRequest,
        }),
        "blocked"
    );
}

#[test]
fn status_notification_skips_states_that_are_not_actionable() {
    assert!(
        status_notification(
            "Claude",
            "build",
            &CLIAgentSessionStatus::InProgress,
            "",
            "http://x/?token=t"
        )
        .is_none()
    );
    assert!(
        status_notification(
            "Claude",
            "build",
            &CLIAgentSessionStatus::Cancelled,
            "",
            "http://x/?token=t"
        )
        .is_none()
    );
}

#[test]
fn status_notification_includes_pending_question_and_session() {
    let blocked = CLIAgentSessionStatus::Blocked {
        message: Some("允许执行 rm -rf?".to_owned()),
        source: BlockedSource::PermissionRequest,
    };
    let text = status_notification(
        "Claude",
        "build（标签 1）",
        &blocked,
        "error: boom at line 3",
        "http://172.16.1.45:9283/?token=t&session=7",
    )
    .unwrap();
    assert!(text.contains("等待授权"), "{text}");
    assert!(text.contains("允许执行 rm -rf?"));
    assert!(text.contains("build（标签 1）"));
    assert!(text.contains("最近输出"));
    assert!(text.contains("error: boom at line 3"));
    assert!(text.contains("session=7"));

    let success = status_notification(
        "Claude",
        "",
        &CLIAgentSessionStatus::Success,
        "",
        "http://x/?token=t",
    )
    .unwrap();
    assert!(success.starts_with("【Claude】"), "{success}");
    assert!(
        !success.contains("最近输出"),
        "success omits tail: {success}"
    );
}

#[test]
fn describe_status_reports_failure_detail() {
    let failed = CLIAgentSessionStatus::Failed {
        error_type: None,
        message: Some("exit 1".to_owned()),
    };
    assert_eq!(describe_status(&failed), "失败：exit 1");
    assert_eq!(describe_status(&CLIAgentSessionStatus::Success), "已完成");
}

#[test]
fn random_wechat_uin_is_base64_of_a_number() {
    let uin = random_wechat_uin();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(uin.as_bytes())
        .expect("valid base64");
    let number = String::from_utf8(decoded).expect("utf8 digits");
    assert!(number.parse::<u32>().is_ok(), "decodes to a u32: {number}");
}
