use std::collections::HashMap;

use insights_bot_telegram_rs::services::rich_recap::{
    CondensedExecutionTrace, ConditionalModelExecutionTrace, GenerationModelExecutionTrace,
    RecapExecutionTrace, build_rich_recap_model_info, escape_rich_markdown_text,
    resolve_rich_recap_references, sanitize_condensed_recap_markdown,
    sanitize_detailed_recap_markdown, unescape_rich_markdown_text,
};

#[test]
fn escaping_user_text_preserves_plain_content_without_active_markup() {
    assert_eq!(
        escape_rich_markdown_text("Group [alpha]_*#\nnext"),
        r"Group \[alpha\]\_\*\# next"
    );
}

#[test]
fn unescaping_reverses_every_character_escaped_by_go() {
    let original = r"plain \*_{}[]()#+-.!|><~` text";
    assert_eq!(
        unescape_rich_markdown_text(&escape_rich_markdown_text(original)),
        original
    );
}

#[test]
fn references_use_only_whitelisted_supergroup_messages() {
    let virtual_to_real =
        HashMap::from([(1, 101), (2, 202), (3, 303), (4, 404), (5, 505), (6, 606)]);

    let resolved = resolve_rich_recap_references(
        "Decision {{tg-ref:1,99,2,1,bad,3,4,5,6}}",
        -100123456789,
        "supergroup",
        &virtual_to_real,
    );

    assert_eq!(
        resolved,
        "Decision [1](https://t.me/c/123456789/101) \
[2](https://t.me/c/123456789/202) \
[3](https://t.me/c/123456789/303) \
[4](https://t.me/c/123456789/404) \
[5](https://t.me/c/123456789/505)"
    );
    assert!(!resolved.contains("606"));
    assert!(!resolved.contains("tg-ref"));
}

#[test]
fn references_are_removed_for_chat_types_without_private_supergroup_links() {
    for chat_type in ["private", "group", "channel"] {
        let resolved = resolve_rich_recap_references(
            "Decision {{tg-ref:1}} and unknown {{tg-ref:99,bad}}.",
            -100123456789,
            chat_type,
            &HashMap::from([(1, 101)]),
        );

        assert!(!resolved.contains("tg-ref"), "chat type {chat_type}");
        assert!(
            !resolved.contains("https://t.me/c/"),
            "chat type {chat_type}"
        );
    }
}

#[test]
fn empty_and_unknown_reference_markers_are_removed() {
    let resolved = resolve_rich_recap_references(
        "Empty {{tg-ref:}} unknown {{tg-ref:7}} end",
        -100123456789,
        "supergroup",
        &HashMap::from([(1, 101)]),
    );

    assert!(!resolved.contains("tg-ref"));
    assert!(!resolved.contains("https://t.me/c/"));
}

#[test]
fn detailed_sanitizer_keeps_structure_and_only_controlled_links() {
    let raw = "## Topic\n\n<script>alert</script>\n\n- **Decision** \
[external](https://example.com) {{tg-ref:1}}\n\n> quoted\n\n```go\ncode()\n```\n\n\
https://example.org/raw";
    let sanitized = sanitize_detailed_recap_markdown(raw);
    let resolved = resolve_rich_recap_references(
        &sanitized,
        -100123456789,
        "supergroup",
        &HashMap::from([(1, 101)]),
    );

    assert!(resolved.contains("## Topic"));
    assert!(resolved.contains("- **Decision** external [1](https://t.me/c/123456789/101)"));
    assert!(resolved.contains("quoted"));
    assert!(resolved.contains("code()"));
    for forbidden in [
        "<script>",
        "</script>",
        "```",
        "https://example.com",
        "https://example.org",
    ] {
        assert!(!resolved.contains(forbidden), "found {forbidden}");
    }
}

#[test]
fn detailed_sanitizer_neutralizes_mentions_but_preserves_emails() {
    let raw = "## Topic\n\n\
- @alice and @bob_dev pinged @ChannelName\n\
- (@carol) replied, 感謝@eve 支援\n\
- @@dave escalated\n\
@lead_1 opened the thread\n\n\
contact admin@example.com for access";

    let sanitized = sanitize_detailed_recap_markdown(raw);

    for mention in [
        "@alice",
        "@bob_dev",
        "@ChannelName",
        "@carol",
        "@eve",
        "@dave",
        "@lead_1",
    ] {
        assert!(!sanitized.contains(mention), "found {mention}");
    }
    assert!(sanitized.contains("- alice and bob_dev pinged ChannelName"));
    assert!(sanitized.contains("- (carol) replied, 感謝eve 支援"));
    assert!(sanitized.contains("- dave escalated"));
    assert!(sanitized.contains("lead_1 opened the thread"));
    assert!(sanitized.contains("admin@example.com"));
}

#[test]
fn detailed_sanitizer_uses_go_ascii_whitespace_classes_for_urls() {
    let raw = "https://example.test\u{00a0}保留文字";

    assert_eq!(sanitize_detailed_recap_markdown(raw), "");
}

#[test]
fn condensed_sanitizer_preserves_visual_structure_and_neutralizes_mentions() {
    let raw = "# 測試主題\n\n\
* **參與者**：@alice、@bob_dev\n\
- 結論由 @@carol 拍板\n\n\
| 指標 | 數值 |\n| --- | --- |\n| 次數 | 4 |\n\n\
<a href=\"https://evil.example\">補充</a>\n\n\
回報請寄 support@example.com";

    let sanitized = sanitize_condensed_recap_markdown(raw);

    assert!(sanitized.contains("### 測試主題"));
    assert!(sanitized.contains("- **參與者**：alice、bob_dev"));
    assert!(sanitized.contains("- 結論由 carol 拍板"));
    assert!(sanitized.contains("| 指標 | 數值 |"));
    assert!(sanitized.contains("| 次數 | 4 |"));
    assert!(sanitized.contains("補充"));
    assert!(sanitized.contains("support@example.com"));
    for forbidden in ["@alice", "@bob_dev", "@carol", "<a", "https://evil.example"] {
        assert!(!sanitized.contains(forbidden), "found {forbidden}");
    }
}

#[test]
fn model_info_shows_configured_check_model_in_standby() {
    let condensed = CondensedExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_model: "condensedModel".into(),
            ..Default::default()
        },
        check: ConditionalModelExecutionTrace {
            generation: GenerationModelExecutionTrace {
                primary_model: "checkModel".into(),
                ..Default::default()
            },
            ..Default::default()
        },
    };
    let recap = RecapExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_model: "detailModel".into(),
            ..Default::default()
        },
    };

    let info = build_rich_recap_model_info(Some(&condensed), Some(&recap));

    assert_eq!(
        info,
        "> **模型資訊**\n>\n> - 濃縮總結：condensedModel\n\
> - 詳細總結：detailModel\n> - Check：checkModel"
    );
    for forbidden in ["完成", "待命", "🤖", "✅"] {
        assert!(!info.contains(forbidden));
    }
}

#[test]
fn model_info_shows_the_check_backup_that_repaired_output() {
    let condensed = CondensedExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_model: "condensedModel".into(),
            ..Default::default()
        },
        check: ConditionalModelExecutionTrace {
            attempted: true,
            succeeded: true,
            generation: GenerationModelExecutionTrace {
                primary_model: "checkPrimary".into(),
                primary_failed: true,
                backup_model: "checkBackup".into(),
                backup_used: true,
                backup_used_model: "checkBackup".into(),
                backup_succeeded: true,
                ..Default::default()
            },
            ..Default::default()
        },
    };
    let recap = RecapExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_model: "detailModel".into(),
            ..Default::default()
        },
    };

    let info = build_rich_recap_model_info(Some(&condensed), Some(&recap));

    assert!(info.contains("> - Check：checkBackup"));
    assert!(!info.contains("修復"));
    assert!(!info.contains("備用"));
    assert_eq!(info.lines().count(), 5);
}

#[test]
fn model_info_treats_backup_only_check_configuration_as_unset() {
    let condensed = CondensedExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_model: "condensedModel".into(),
            ..Default::default()
        },
        check: ConditionalModelExecutionTrace {
            generation: GenerationModelExecutionTrace {
                backup_model: "checkBackup".into(),
                ..Default::default()
            },
            ..Default::default()
        },
    };

    let info = build_rich_recap_model_info(Some(&condensed), None);

    assert!(info.contains("> - Check：未設定"));
    assert!(!info.contains("> - Check：checkBackup"));
}

#[test]
fn model_info_lists_every_model_that_produced_detail_slices_once() {
    let condensed = CondensedExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_used_model: "condensedModel".into(),
            ..Default::default()
        },
        ..Default::default()
    };
    let recap = RecapExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_used_model: "detailPrimary".into(),
            backup_used: true,
            backup_used_model: "detailBackup".into(),
            backup_succeeded: true,
            ..Default::default()
        },
    };

    let info = build_rich_recap_model_info(Some(&condensed), Some(&recap));

    assert!(info.contains("> - 詳細總結：detailPrimary、detailBackup"));
}

#[test]
fn model_info_prefers_the_resolved_successful_check_model() {
    let condensed = CondensedExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_used_model: "condensedModel".into(),
            ..Default::default()
        },
        check: ConditionalModelExecutionTrace {
            attempted: true,
            succeeded: true,
            generation: GenerationModelExecutionTrace {
                primary_model: "checkAlias".into(),
                primary_used_model: "resolvedCheck".into(),
                ..Default::default()
            },
            ..Default::default()
        },
    };

    let info = build_rich_recap_model_info(Some(&condensed), None);

    assert!(info.contains("> - Check：resolvedCheck"));
    assert!(!info.contains("> - Check：checkAlias"));
}

#[test]
fn model_info_hides_a_failed_backup_model() {
    let condensed = CondensedExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_model: "condensedPrimary".into(),
            primary_failed: true,
            backup_model: "condensedBackup".into(),
            backup_used: true,
            backup_used_model: "resolvedFailedBackup".into(),
            backup_succeeded: false,
            ..Default::default()
        },
        ..Default::default()
    };

    let info = build_rich_recap_model_info(Some(&condensed), None);

    assert!(info.contains("> - 濃縮總結：資訊不可用"));
    assert!(!info.contains("resolvedFailedBackup"));
}

#[test]
fn successful_check_repair_attributes_the_condensed_backup_source() {
    let condensed = CondensedExecutionTrace {
        generation: GenerationModelExecutionTrace {
            primary_model: "condensedPrimary".into(),
            primary_failed: true,
            backup_model: "condensedBackup".into(),
            backup_used: true,
            backup_used_model: "resolvedBackupSource".into(),
            backup_succeeded: false,
            ..Default::default()
        },
        check: ConditionalModelExecutionTrace {
            attempted: true,
            succeeded: true,
            generation: GenerationModelExecutionTrace {
                primary_model: "checkAlias".into(),
                primary_used_model: "resolvedCheck".into(),
                ..Default::default()
            },
            ..Default::default()
        },
    };

    let info = build_rich_recap_model_info(Some(&condensed), None);

    assert!(info.contains("> - 濃縮總結：resolvedBackupSource"));
    assert!(info.contains("> - Check：resolvedCheck"));
}
