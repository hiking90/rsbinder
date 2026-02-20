fn demo_parse(label: &str, src: &str) {
    let ctx = rsbinder_aidl::SourceContext {
        filename: format!("{label}.aidl"),
        source: src.to_string(),
    };
    match rsbinder_aidl::parse_document(&ctx) {
        Err(e) => {
            eprintln!("──── {} ────", label);
            eprintln!("{:?}\n", miette::Report::new(e));
        }
        Ok(_) => eprintln!("── {} ── (no parse error)\n", label),
    }
}

fn demo_builder(label: &str, src: &str) {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join(format!("{label}.aidl"));
    let output = dir.path().join(format!("{label}.rs"));
    std::fs::write(&input, src).unwrap();

    let result = rsbinder_aidl::Builder::new()
        .source(input)
        .output(output)
        .generate();

    if let Err(e) = result {
        eprintln!("──── {} ────", label);
        eprintln!("{:?}\n", miette::Report::new(e));
    }
}

fn main() {
    // ── 구문 에러 (parse 단계) ──────────────────────────

    // 1) 숫자로 시작하는 식별자
    demo_parse(
        "syntax_error",
        "interface IHello {\n    void 123bad();\n}",
    );

    // 2) 세미콜론 누락
    demo_parse(
        "missing_semicolon",
        "parcelable Foo {\n    int field\n}",
    );

    // 3) 닫는 중괄호 누락 (EOF)
    demo_parse(
        "unclosed_brace",
        "interface IBar {\n    void ok();\n",
    );

    // ── 시맨틱 에러 (generator 단계, Builder 경유) ───────

    // 4) 중복 transaction code
    demo_builder(
        "duplicate_transaction_code",
        "interface IDup {\n    void m1() = 10;\n    void m2() = 10;\n}",
    );

    // 5) mixed explicit/implicit transaction IDs
    demo_builder(
        "mixed_transaction_ids",
        "interface IMixed {\n    void m1() = 1;\n    void m2();\n}",
    );

    // 6) List에 Generic 없음
    demo_builder(
        "list_without_generic",
        "parcelable Foo {\n    List items;\n}",
    );

    // 7) @nullable primitive
    demo_builder(
        "nullable_primitive",
        "parcelable Foo {\n    @nullable int val;\n}",
    );
}
