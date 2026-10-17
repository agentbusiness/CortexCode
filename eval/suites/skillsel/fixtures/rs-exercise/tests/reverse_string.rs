use reverse_string::reverse;

#[test]
fn an_empty_string() {
    assert_eq!(reverse(""), "");
}

#[test]
#[ignore]
fn a_word() {
    assert_eq!(reverse("robot"), "tobor");
}

#[test]
#[ignore]
fn wide_characters() {
    assert_eq!(reverse("子猫"), "猫子");
}
