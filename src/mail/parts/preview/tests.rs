use super::*;

fn preview(text: &str, truncated: bool) -> Preview {
    Preview::Text {
        text: text.into(),
        truncated,
    }
}

#[test]
fn cap_counts_unicode_scalar_values_and_drops_the_trailing_separator() {
    let mut output = PreviewText::default();
    assert!(!output.append(&"😀".repeat(DISPLAY_CODE_POINTS - 1)));
    assert!(output.append("\r\n中"));
    assert_eq!(
        output.finish(true),
        preview(&"😀".repeat(DISPLAY_CODE_POINTS - 1), true)
    );
}
