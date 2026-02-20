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
    // ── Syntax errors (parse phase) ─────────────────────

    // 1) identifier starting with a digit
    demo_parse("syntax_error", "interface IHello {\n    void 123bad();\n}");

    // 2) missing semicolon
    demo_parse("missing_semicolon", "parcelable Foo {\n    int field\n}");

    // 3) missing closing brace (EOF)
    demo_parse("unclosed_brace", "interface IBar {\n    void ok();\n");

    // ── Semantic errors (generator phase, via Builder) ───

    // 4) duplicate transaction code
    demo_builder(
        "duplicate_transaction_code",
        "interface IDup {\n    void m1() = 10;\n    void m2() = 10;\n}",
    );

    // 5) mixed explicit/implicit transaction IDs
    demo_builder(
        "mixed_transaction_ids",
        "interface IMixed {\n    void m1() = 1;\n    void m2();\n}",
    );

    // 6) List without a Generic type argument
    demo_builder(
        "list_without_generic",
        "parcelable Foo {\n    List items;\n}",
    );

    // 7) @nullable primitive
    demo_builder(
        "nullable_primitive",
        "parcelable Foo {\n    @nullable int val;\n}",
    );

    // ── New: direction_span / name_span ──────────────────

    // 8) out primitive: direction_span improvement (points to the 'out' keyword)
    demo_builder(
        "out_primitive",
        "interface IFoo {\n    void foo(out int x);\n}",
    );

    // 9) inout String: direction_span improvement (points to the 'inout' keyword)
    demo_builder(
        "inout_string",
        "interface IFoo {\n    void bar(inout String s);\n}",
    );

    // 10) enum invalid backing type: EnumDecl.name_span → points to the enum name
    demo_builder(
        "enum_bad_backing",
        "package foo;\n@Backing(type=\"List\")\nenum MyEnum { V1 = 1, V2 = 2 }",
    );
}
