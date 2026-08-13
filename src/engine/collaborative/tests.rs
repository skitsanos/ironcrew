use super::*;

#[test]
fn transcript_rejects_growth_before_allocating_an_oversized_entry() {
    let mut transcript = Transcript::new(32);
    transcript.push("task", "Task: ", "small").unwrap();
    let error = transcript
        .push("task", "[agent]: ", &"x".repeat(64))
        .expect_err("oversized turn must fail");
    assert!(error.to_string().contains("transcript exceeds"));
    assert_eq!(transcript.entries.len(), 1);
    assert!(transcript.bytes <= 32);
}

#[test]
fn prompt_builder_stops_at_character_budget_without_splitting_utf8() {
    let mut transcript = Transcript::new(1024);
    transcript.push("task", "", &"é".repeat(100)).unwrap();
    let (prompt, truncated) = build_bounded_prompt("", &transcript, 17);
    assert!(truncated);
    assert_eq!(prompt.chars().count(), 17);
    assert_eq!(prompt.len(), 34);
}
