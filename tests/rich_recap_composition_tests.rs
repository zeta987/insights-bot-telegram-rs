use insights_bot_telegram_rs::services::rich_recap::{
    CondensedExecutionTrace, ConditionalModelExecutionTrace, GenerationModelExecutionTrace,
    RICH_MESSAGE_UTF16_UNIT_LIMIT, RecapExecutionTrace, RichRecapSummaryConfig,
    build_rich_recap_summary, compose_rich_recap_messages, fallback_condensed_summary,
    rich_markdown_to_plain_text, split_plain_text,
};

const PLAIN_MESSAGE_UTF16_UNIT_LIMIT: usize = 4_096;
const DETAILS_OPEN: &str = "<details><summary>詳細總結</summary>\n\n";
const DETAILS_CLOSE: &str = "\n\n</details>";

fn utf16_units(text: &str) -> usize {
    text.encode_utf16().count()
}

#[test]
fn composition_places_condensed_content_before_collapsible_details() {
    let messages = compose_rich_recap_messages(
        "濃縮摘要 ✨",
        &["## 主題\n\n詳細內容 [1](https://t.me/c/123/456)".to_owned()],
    );

    assert_eq!(
        messages,
        vec![
            "濃縮摘要 ✨\n\n<details><summary>詳細總結</summary>\n\n\
## 主題\n\n詳細內容 [1](https://t.me/c/123/456)\n\n</details>"
        ]
    );
}

#[test]
fn composition_splits_at_markdown_block_boundaries() {
    let first_block = format!("## Topic A\n\n{}", "a".repeat(17_000));
    let second_block = format!("## Topic B\n\n{}", "b".repeat(17_000));
    let messages = compose_rich_recap_messages(
        "Condensed summary",
        &[format!("{first_block}\n\n{second_block}")],
    );

    assert_eq!(messages.len(), 2);
    for message in &messages {
        assert!(utf16_units(message) <= RICH_MESSAGE_UTF16_UNIT_LIMIT);
        assert_eq!(message.matches(DETAILS_OPEN).count(), 1);
        assert_eq!(message.matches("</details>").count(), 1);
    }
    assert!(messages[0].contains("Condensed summary"));
    assert!(messages[0].contains("## Topic A"));
    assert!(!messages[0].contains("## Topic B"));
    assert!(!messages[1].contains("Condensed summary"));
    assert!(messages[1].contains("## Topic B"));
}

#[test]
fn composition_counts_cjk_by_telegram_utf16_units() {
    let messages =
        compose_rich_recap_messages("摘要", &["界".repeat(RICH_MESSAGE_UTF16_UNIT_LIMIT)]);

    assert_eq!(messages.len(), 2);
    for message in messages {
        assert!(utf16_units(&message) <= RICH_MESSAGE_UTF16_UNIT_LIMIT);
        assert_eq!(message.matches(DETAILS_OPEN).count(), 1);
        assert_eq!(message.matches("</details>").count(), 1);
    }
}

#[test]
fn composition_counts_non_bmp_characters_as_two_utf16_units() {
    let emoji_count = RICH_MESSAGE_UTF16_UNIT_LIMIT / 2;
    let messages = compose_rich_recap_messages("", &["🧾".repeat(emoji_count)]);

    assert!(messages.len() >= 2);
    let mut delivered = 0;
    for message in &messages {
        assert!(utf16_units(message) <= RICH_MESSAGE_UTF16_UNIT_LIMIT);
        delivered += message.matches('🧾').count();
    }
    assert_eq!(delivered, emoji_count);
}

#[test]
fn composition_keeps_the_exact_utf16_boundary_in_one_message() {
    let wrapper_units = utf16_units(&format!("{DETAILS_OPEN}{DETAILS_CLOSE}"));
    let boundary_emoji_count = (RICH_MESSAGE_UTF16_UNIT_LIMIT - wrapper_units) / 2;

    let messages = compose_rich_recap_messages("", &["🧾".repeat(boundary_emoji_count)]);
    assert_eq!(messages.len(), 1);
    assert_eq!(utf16_units(&messages[0]), RICH_MESSAGE_UTF16_UNIT_LIMIT);

    let oversized = compose_rich_recap_messages("", &["🧾".repeat(boundary_emoji_count + 1)]);
    assert_eq!(oversized.len(), 2);
    assert!(
        oversized
            .iter()
            .all(|message| utf16_units(message) <= RICH_MESSAGE_UTF16_UNIT_LIMIT)
    );
}

#[test]
fn composition_does_not_split_a_telegram_citation_link() {
    let citation = "[1](https://t.me/c/123/456)";
    let wrapper_units = utf16_units(&format!("{DETAILS_OPEN}{DETAILS_CLOSE}"));
    let condensed_prefix_units = utf16_units("摘要\n\n");
    let first_body_limit = RICH_MESSAGE_UTF16_UNIT_LIMIT - wrapper_units - condensed_prefix_units;
    let detail = format!(
        "{} {citation}{}",
        "a".repeat(first_body_limit - 10),
        "b".repeat(20)
    );

    let messages = compose_rich_recap_messages("摘要", &[detail]);

    assert_eq!(messages.len(), 2);
    assert_eq!(messages.join("").matches(citation).count(), 1);
    assert!(!messages[0].contains("https://"));
    assert!(messages[1].contains(citation));
    assert!(
        messages
            .iter()
            .all(|message| utf16_units(message) <= RICH_MESSAGE_UTF16_UNIT_LIMIT)
    );
}

#[test]
fn composition_handles_missing_sections() {
    assert_eq!(
        compose_rich_recap_messages("Only condensed", &[]),
        vec!["Only condensed"]
    );
    assert!(compose_rich_recap_messages("", &[]).is_empty());
}

#[test]
fn oversized_condensed_content_is_emitted_before_detail_containers() {
    let messages = compose_rich_recap_messages(
        &"濃".repeat(RICH_MESSAGE_UTF16_UNIT_LIMIT + 1),
        &["Detailed summary".to_owned()],
    );

    assert_eq!(messages.len(), 3);
    assert!(!messages[0].contains("<details>"));
    assert!(!messages[1].contains("<details>"));
    assert!(messages[2].contains(DETAILS_OPEN));
    assert!(
        messages
            .iter()
            .all(|message| utf16_units(message) <= RICH_MESSAGE_UTF16_UNIT_LIMIT)
    );
}

#[test]
fn fallback_keeps_short_summaries_joined_in_order() {
    assert_eq!(
        fallback_condensed_summary(
            &["第一段總結".to_owned(), "第二段總結".to_owned()],
            "預設摘要"
        ),
        "第一段總結 第二段總結"
    );
}

#[test]
fn fallback_uses_the_default_when_summaries_are_blank() {
    assert_eq!(
        fallback_condensed_summary(
            &["   ".to_owned(), "\n\t".to_owned()],
            "過去 4 小時的群組聊天回顧"
        ),
        "過去 4 小時的群組聊天回顧"
    );
    assert_eq!(fallback_condensed_summary(&[], " 預設摘要 "), "預設摘要");
}

#[test]
fn fallback_drops_a_citation_that_would_be_sliced() {
    let prefix = "聊".repeat(100);
    let citation = "[1](https://t.me/c/1234567890/12345)";
    let result = fallback_condensed_summary(&[format!("{prefix}{citation}")], "預設摘要");

    assert_eq!(result, prefix);
    assert!(!result.contains("[1]("));
    assert!(utf16_units(&result) <= 120);
}

#[test]
fn fallback_drops_an_emphasis_span_that_would_be_sliced() {
    let prefix = "聊".repeat(115);
    let result = fallback_condensed_summary(&[format!("{prefix}**重點內容**")], "預設摘要");

    assert_eq!(result, prefix);
    assert!(!result.contains('*'));
}

#[test]
fn fallback_prefers_a_natural_sentence_boundary() {
    let first = format!("{}。", "很".repeat(80));
    let second = "多".repeat(80);
    let result = fallback_condensed_summary(&[first.clone(), second], "預設摘要");

    assert_eq!(result, first);
    assert!(utf16_units(&result) <= 120);
}

#[test]
fn fallback_measures_non_bmp_text_in_utf16_units() {
    let result = fallback_condensed_summary(&["🧾".repeat(61)], "預設摘要");

    assert_eq!(result, "🧾".repeat(60));
    assert_eq!(utf16_units(&result), 120);
}

#[test]
fn summary_builder_uses_clickable_initiator_and_fixed_hierarchy() {
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
    let summary = build_rich_recap_summary(&RichRecapSummaryConfig {
        title: "TG BOT [測試]",
        hours: 1,
        initiator_name: "TestUser(測試)",
        initiator_user_id: 42,
        condensed_summary: "濃縮總結 🤖 ### 測試主題\n\n- **觸發方式**：/recap",
        condensed_trace: Some(&condensed),
        recap_trace: Some(&recap),
        ..Default::default()
    });

    assert_eq!(
        summary,
        "# 【TG BOT \\[測試\\]】聊天回顧\n\n\
_用戶 [TestUser\\(測試\\)](tg://user?id=42) 發起 **1 小時**總結_\n\n\
## 濃縮總結\n\n\
### 測試主題\n\n\
- **觸發方式**：/recap\n\n\
---\n\n\
> **模型資訊**\n>\n\
> - 濃縮總結：condensedModel\n\
> - 詳細總結：detailModel\n\
> - Check：checkModel"
    );
}

#[test]
fn summary_builder_adds_automatic_subscription_and_group_notices() {
    let summary = build_rich_recap_summary(&RichRecapSummaryConfig {
        title: "TG BOT 測試",
        hours: 4,
        automatic: true,
        condensed_summary: "摘要",
        general_group_notice: true,
        subscription_chat_title: "TG BOT 測試",
        ..Default::default()
    });

    assert!(summary.contains("_自動產生 **4 小時**總結_"));
    assert!(summary.contains("# 【TG BOT 測試】聊天回顧"));
    assert!(summary.contains("> 📬 這是您訂閱的 **TG BOT 測試** 群組定時聊天回顧。"));
    assert!(
        summary
            .contains("> 💡 一般群組來源暫時不顯示原訊息引用；升級為 supergroup 後即可建立連結。")
    );
    assert!(!summary.contains("#recap"));
}

#[test]
fn summary_builder_preserves_complete_bracketed_group_names() {
    let already_bracketed = build_rich_recap_summary(&RichRecapSummaryConfig {
        title: "【每日聊天回顧】",
        ..Default::default()
    });
    let ordinary = build_rich_recap_summary(&RichRecapSummaryConfig {
        title: "聊天回顧",
        ..Default::default()
    });
    let empty = build_rich_recap_summary(&RichRecapSummaryConfig::default());

    assert!(already_bracketed.starts_with("# 【每日聊天回顧】聊天回顧"));
    assert!(ordinary.starts_with("# 【聊天回顧】聊天回顧"));
    assert!(empty.starts_with("# 聊天回顧"));
}

#[test]
fn plain_text_keeps_user_labels_without_tg_targets() {
    let rich = "# 🧾 聊天回顧\n\n_用戶 [TestUser](tg://user?id=42) 發起 **1 小時**總結_";

    assert_eq!(
        rich_markdown_to_plain_text(rich),
        "🧾 聊天回顧\n\n用戶 TestUser 發起 1 小時總結"
    );
}

#[test]
fn plain_text_conversion_preserves_visible_details_and_http_targets() {
    let rich = "# 標題\n\n> quote\n\n<details><summary>詳細總結</summary>\n\n\
**粗體** [來源](https://example.com/x) `code`\n\n</details>";

    assert_eq!(
        rich_markdown_to_plain_text(rich),
        "標題\n\nquote\n\n詳細總結\n\n粗體 來源 (https://example.com/x) code"
    );
}

#[test]
fn plain_text_preserves_go_bare_cr_and_uppercase_close_tag_quirks() {
    let rich = "<DETAILS><SUMMARY>X</SUMMARY>body</DETAILS>\rnext";

    assert_eq!(
        rich_markdown_to_plain_text(rich),
        "X\n\nbody</DETAILS>\rnext"
    );
}

#[test]
fn plain_text_uses_go_ascii_whitespace_and_digit_classes() {
    let rich = "<details>\u{00a0}<summary>X</summary>body</details>\n\
[來源](https://example.com/a\u{00a0}b)\n\
[User](tg://user?id=١٢٣)";

    assert_eq!(
        rich_markdown_to_plain_text(rich),
        "<details>\u{00a0}<summary>X</summary>body\n\
來源 (https://example.com/a\u{00a0}b)\n\
[User](tg://user?id=١٢٣)"
    );
}

#[test]
fn plain_split_keeps_citation_label_and_url_together() {
    let citation = "1 (https://t.me/c/123/456)";
    let text = format!(
        "{} {citation}",
        "a".repeat(PLAIN_MESSAGE_UTF16_UNIT_LIMIT - 10)
    );
    let parts = split_plain_text(&text, PLAIN_MESSAGE_UTF16_UNIT_LIMIT);

    assert_eq!(parts.len(), 2);
    assert!(!parts[0].contains("https://"));
    assert_eq!(parts[1], citation);
    assert!(
        parts
            .iter()
            .all(|part| utf16_units(part) <= PLAIN_MESSAGE_UTF16_UNIT_LIMIT)
    );
}

#[test]
fn plain_split_counts_non_bmp_characters_as_two_utf16_units() {
    let text = "🧾".repeat(PLAIN_MESSAGE_UTF16_UNIT_LIMIT / 2 + 1);
    let parts = split_plain_text(&text, PLAIN_MESSAGE_UTF16_UNIT_LIMIT);

    assert_eq!(parts.len(), 2);
    assert!(
        parts
            .iter()
            .all(|part| utf16_units(part) <= PLAIN_MESSAGE_UTF16_UNIT_LIMIT)
    );
    assert_eq!(parts.concat(), text);
}

#[test]
fn plain_split_keeps_the_exact_utf16_boundary_in_one_part() {
    let text = "🧾".repeat(PLAIN_MESSAGE_UTF16_UNIT_LIMIT / 2);
    assert_eq!(
        split_plain_text(&text, PLAIN_MESSAGE_UTF16_UNIT_LIMIT),
        vec![text]
    );
}

#[test]
fn plain_split_keeps_a_leading_citation_whole_when_it_fits() {
    let citation = format!("{} (https://t.me/c/123/456)", "x".repeat(32));
    let tail = "y".repeat(20);
    let text = format!("{citation}{tail}");

    assert_eq!(split_plain_text(&text, 60), vec![citation, tail]);
}

#[test]
fn plain_split_treats_nbsp_as_non_whitespace_like_go() {
    let citation = "A\u{00a0}B (https://x.co)";
    let text = format!("{}\u{000c}{citation}", "x".repeat(18));

    assert_eq!(
        split_plain_text(&text, 20),
        vec!["x".repeat(18), citation.to_owned()]
    );
}

#[test]
fn plain_split_returns_empty_for_blank_text_or_non_positive_budget() {
    assert!(split_plain_text(" \n\t ", 100).is_empty());
    assert!(split_plain_text("content", 0).is_empty());
}

#[test]
fn plain_split_preserves_go_progress_for_a_scalar_wider_than_the_budget() {
    assert_eq!(split_plain_text("🧾", 1), vec!["🧾"]);
}
