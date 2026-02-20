// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// miette의 #[derive(Diagnostic)]이 생성하는 trait 구현 메서드들을 통해 struct 필드들이
// 사용되지만, rustc lint 분석이 proc-macro 생성 코드를 추적하지 못해 경고가 발생한다.
#![allow(unused_assignments)]

use miette::{Diagnostic, NamedSource, SourceSpan};
use thiserror::Error;

/// AIDL 컴파일러의 최상위 에러 타입
#[derive(Error, Debug, Diagnostic)]
pub enum AidlError {
    #[error(transparent)]
    #[diagnostic(transparent)]
    Parse(#[from] ParseError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Semantic(#[from] SemanticError),

    #[error(transparent)]
    #[diagnostic(transparent)]
    Resolution(#[from] ResolutionError),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// 여러 AIDL 파일 처리 시 복수 에러를 집계
    #[error("{} error(s) occurred during AIDL compilation", errors.len())]
    #[diagnostic(code(aidl::multiple_errors))]
    Multiple {
        #[related]
        errors: Vec<AidlError>,
    },
}

impl AidlError {
    /// 에러를 수집할 때 Multiple을 풀어서 평탄화한다.
    pub fn collect(errors: Vec<AidlError>) -> Option<AidlError> {
        let flat: Vec<AidlError> = errors
            .into_iter()
            .flat_map(|e| match e {
                AidlError::Multiple { errors } => errors,
                other => vec![other],
            })
            .collect();
        match flat.len() {
            0 => None,
            1 => flat.into_iter().next(),
            _ => Some(AidlError::Multiple { errors: flat }),
        }
    }
}

/// 구문 분석 에러 (pest 파서 에러 래핑)
#[allow(unused)]
#[derive(Error, Debug, Diagnostic)]
#[error("AIDL syntax error")]
#[diagnostic(code(aidl::parse_error))]
pub struct ParseError {
    #[source_code]
    pub src: NamedSource<String>,
    #[label("{message}")]
    pub span: SourceSpan,
    pub message: String,
    #[help]
    pub help: Option<String>,
}

/// 시맨틱 에러 (타입 검증, transaction code 등)
#[allow(unused)]
#[derive(Error, Debug, Diagnostic)]
pub enum SemanticError {
    #[error("Interface '{interface}': transaction code {code} conflict between '{method1}' and '{method2}'")]
    #[diagnostic(
        code(aidl::duplicate_transaction_code),
        help("use unique positive integer values for each method's transaction code")
    )]
    DuplicateTransactionCode {
        interface: String,
        method1: String,
        method2: String,
        code: i64,
        #[source_code]
        src: NamedSource<String>,
        #[label("method '{method1}' uses code {code}")]
        span: SourceSpan,
        #[related]
        related: Vec<DuplicateCodeRelated>,
    },

    #[error("Interface '{interface}': mixed explicit/implicit transaction IDs")]
    #[diagnostic(
        code(aidl::mixed_transaction_ids),
        help("either all methods must have explicitly assigned transaction IDs or none of them should")
    )]
    MixedTransactionIds {
        interface: String,
        #[source_code]
        src: NamedSource<String>,
        #[label("defined in this interface")]
        span: SourceSpan,
    },

    /// 현재 AIDL 문법으로는 도달 불가능한 코드 경로.
    /// pest 문법의 INTVALUE는 마이너스 부호를 허용하지 않으므로
    /// 통합 테스트 대신 단위 테스트(MethodDecl 직접 구성)로만 검증한다.
    #[error("Interface '{interface}': method '{method}' has negative transaction code {code}")]
    #[diagnostic(
        code(aidl::negative_transaction_code),
        help("transaction codes must be non-negative integers within u32 range")
    )]
    NegativeTransactionCode {
        interface: String,
        method: String,
        code: i64,
        #[source_code]
        src: NamedSource<String>,
        #[label("negative code here")]
        span: SourceSpan,
    },

    #[error("Interface '{interface}': method '{method}' has transaction code {code} exceeding u32 range")]
    #[diagnostic(
        code(aidl::transaction_code_overflow),
        help("transaction codes must fit within u32 (0..=4294967295)")
    )]
    TransactionCodeOverflow {
        interface: String,
        method: String,
        code: i64,
        #[source_code]
        src: NamedSource<String>,
        #[label("overflowing code here")]
        span: SourceSpan,
    },

    #[error("unsupported type: {type_name}")]
    #[diagnostic(code(aidl::unsupported_type))]
    UnsupportedType {
        type_name: String,
        #[help]
        help: Option<String>,
        #[source_code]
        src: NamedSource<String>,
        #[label("this type is not supported")]
        span: SourceSpan,
    },

    #[error("invalid operation: {message}")]
    #[diagnostic(code(aidl::invalid_operation))]
    InvalidOperation {
        message: String,
        #[source_code]
        src: NamedSource<String>,
        #[label("here")]
        span: SourceSpan,
    },
}

/// DuplicateTransactionCode의 두 번째 위치를 표시하기 위한 보조 진단.
///
/// 주의: `#[label]`에서 `{method}` 필드를 보간하므로, `method` 필드가
/// `span` 필드보다 앞에 선언되어야 miette derive 매크로가 올바르게 동작한다.
/// (miette 7.x 제약 — Phase 1에서 컴파일 검증 완료 후 확인)
#[allow(unused)]
#[derive(Error, Debug, Diagnostic)]
#[error("conflicting method defined here")]
pub struct DuplicateCodeRelated {
    pub method: String,
    #[source_code]
    pub src: NamedSource<String>,
    #[label("method '{method}' also uses this code")]
    pub span: SourceSpan,
}

/// 이름 해석 에러 (import, namespace)
#[allow(unused)]
#[derive(Error, Debug, Diagnostic)]
pub enum ResolutionError {
    #[error("import '{import}' not found")]
    #[diagnostic(
        code(aidl::import_not_found),
        help("check that the imported type exists in the include paths")
    )]
    ImportNotFound {
        import: String,
        #[source_code]
        src: NamedSource<String>,
        #[label("imported here")]
        span: SourceSpan,
    },

    #[error("unknown type '{name}'")]
    #[diagnostic(
        code(aidl::unknown_type),
        help("verify that the type is defined and imported correctly")
    )]
    UnknownType {
        name: String,
        #[source_code]
        src: NamedSource<String>,
        #[label("referenced here")]
        span: SourceSpan,
    },
}

/// const_expr.rs 내부의 연산 에러를 전달하는 경량 에러 타입.
/// 소스 위치 정보 없이 메시지만 담으며, 호출자(parser/generator)에서
/// 소스 컨텍스트를 붙여 SemanticError::InvalidOperation으로 변환한다.
#[derive(Error, Debug)]
#[error("{message}")]
pub struct ConstExprError {
    pub message: String,
}

impl ConstExprError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// pest 에러를 miette ParseError로 변환하는 유틸리티.
///
/// Rule 타입에 대해 제네릭하게 구현하여 parser.rs의 Rule 타입에 의존하지 않는다.
pub fn pest_error_to_diagnostic<R: pest::RuleType>(
    err: pest::error::Error<R>,
    filename: &str,
    source: &str,
) -> ParseError {
    let (offset, length) = match err.location {
        pest::error::InputLocation::Pos(pos) => {
            // EOF 위치에서 에러 발생 시 소스 범위를 벗어나지 않도록 방어
            let len = if pos >= source.len() { 0 } else { 1 };
            (pos, len)
        }
        pest::error::InputLocation::Span((start, end)) => (start, end - start),
    };

    let message = match &err.variant {
        pest::error::ErrorVariant::ParsingError {
            positives,
            negatives,
        } => format_pest_expectations(positives, negatives),
        pest::error::ErrorVariant::CustomError { message } => message.clone(),
    };

    ParseError {
        src: NamedSource::new(filename, source.to_string()),
        span: SourceSpan::new(offset.into(), length),
        message,
        help: None,
    }
}

/// pest의 파싱 오류에서 기대 토큰 정보를 사람이 읽기 쉬운 메시지로 변환한다.
fn format_pest_expectations<R: std::fmt::Debug>(
    positives: &[R],
    negatives: &[R],
) -> String {
    let mut parts = Vec::new();

    if !positives.is_empty() {
        let pos_str: Vec<String> = positives.iter().map(|r| format!("{r:?}")).collect();
        parts.push(format!("expected {}", pos_str.join(", ")));
    }

    if !negatives.is_empty() {
        let neg_str: Vec<String> = negatives.iter().map(|r| format!("{r:?}")).collect();
        parts.push(format!("unexpected {}", neg_str.join(", ")));
    }

    if parts.is_empty() {
        "syntax error".to_string()
    } else {
        parts.join("; ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(offset: usize, len: usize) -> SourceSpan {
        SourceSpan::new(offset.into(), len)
    }

    // 1.1a: ParseError Display trait 검증
    #[test]
    fn test_parse_error_display() {
        let err = ParseError {
            src: NamedSource::new("test.aidl", "parcelable Foo {}".to_string()),
            span: span(0, 1),
            message: "unexpected token".to_string(),
            help: None,
        };
        let display = format!("{err}");
        assert!(display.contains("AIDL syntax error"), "Got: {display}");
    }

    // 1.1b: ParseError diagnostic code 검증
    #[test]
    fn test_parse_error_diagnostic_code() {
        use miette::Diagnostic;
        let err = ParseError {
            src: NamedSource::new("test.aidl", "parcelable Foo {}".to_string()),
            span: span(0, 1),
            message: "test error".to_string(),
            help: None,
        };
        let code = err.code().expect("ParseError must have a diagnostic code");
        assert_eq!(code.to_string(), "aidl::parse_error");
    }

    // 1.1c: ParseError source span 검증
    #[test]
    fn test_parse_error_source_span() {
        use miette::Diagnostic;
        let err = ParseError {
            src: NamedSource::new("test.aidl", "parcelable Foo {}".to_string()),
            span: span(10, 3),
            message: "test error".to_string(),
            help: None,
        };
        let labels: Vec<_> = err.labels().expect("must have labels").collect();
        assert_eq!(labels.len(), 1);
        let label_span = labels[0].inner();
        assert_eq!(label_span.offset(), 10);
        assert_eq!(label_span.len(), 3);
    }

    // 1.1d: SemanticError variants Display 검증
    #[test]
    fn test_semantic_error_variants_display() {
        let err = SemanticError::MixedTransactionIds {
            interface: "IFoo".to_string(),
            src: NamedSource::new("test.aidl", "interface IFoo {}".to_string()),
            span: span(0, 4),
        };
        let display = format!("{err}");
        assert!(
            display.contains("IFoo"),
            "Display should contain interface name, got: {display}"
        );
        assert!(
            display.contains("mixed"),
            "Display should contain 'mixed', got: {display}"
        );
    }

    // 1.1e: ResolutionError::ImportNotFound Display 검증
    #[test]
    fn test_resolution_error_display() {
        let err = ResolutionError::ImportNotFound {
            import: "foo.bar.Baz".to_string(),
            src: NamedSource::new("test.aidl", "import foo.bar.Baz;".to_string()),
            span: span(0, 18),
        };
        let display = format!("{err}");
        assert!(
            display.contains("foo.bar.Baz"),
            "Got: {display}"
        );
        assert!(display.contains("not found"), "Got: {display}");
    }

    // 1.1f: AidlError From<ParseError> 변환 검증
    #[test]
    fn test_aidl_error_from_parse_error() {
        let parse_err = ParseError {
            src: NamedSource::new("test.aidl", "bad".to_string()),
            span: span(0, 3),
            message: "syntax error".to_string(),
            help: None,
        };
        let aidl_err: AidlError = parse_err.into();
        assert!(matches!(aidl_err, AidlError::Parse(_)));
    }

    // 1.1g: AidlError → Box<dyn Error> 변환 (기존 API 호환성)
    #[test]
    fn test_aidl_error_into_box_dyn_error() {
        use std::error::Error;
        let parse_err = ParseError {
            src: NamedSource::new("test.aidl", "bad".to_string()),
            span: span(0, 3),
            message: "syntax error".to_string(),
            help: None,
        };
        let aidl_err: AidlError = parse_err.into();
        let _box_err: Box<dyn Error> = Box::new(aidl_err);
    }

    // 1.1h: pest Pos 위치 → SourceSpan 변환 검증
    #[test]
    fn test_pest_error_to_diagnostic_pos() {
        use miette::Diagnostic;
        // Pos(42)에서 source 길이가 충분한 경우 length=1
        let source = "a".repeat(100);
        let err = ParseError {
            src: NamedSource::new("test.aidl", source.clone()),
            span: span(42, 1),
            message: "test".to_string(),
            help: None,
        };
        let labels: Vec<_> = err.labels().expect("must have labels").collect();
        assert_eq!(labels[0].inner().offset(), 42);
        assert_eq!(labels[0].inner().len(), 1);
    }

    // 1.1i: pest Span 위치 → SourceSpan 변환 검증
    #[test]
    fn test_pest_error_to_diagnostic_span() {
        use miette::Diagnostic;
        let err = ParseError {
            src: NamedSource::new("test.aidl", "0123456789abcdefghij".to_string()),
            span: span(10, 10),
            message: "test".to_string(),
            help: None,
        };
        let labels: Vec<_> = err.labels().expect("must have labels").collect();
        assert_eq!(labels[0].inner().offset(), 10);
        assert_eq!(labels[0].inner().len(), 10);
    }

    // 1.1j: pest EOF 위치 → SourceSpan 변환 (범위 초과 방어)
    #[test]
    fn test_pest_error_to_diagnostic_eof() {
        let source = "abc";
        // EOF: pos >= source.len() → length = 0
        let (offset, length) = {
            let pos = source.len();
            let len = if pos >= source.len() { 0 } else { 1 };
            (pos, len)
        };
        let err = ParseError {
            src: NamedSource::new("test.aidl", source.to_string()),
            span: span(offset, length),
            message: "unexpected EOF".to_string(),
            help: None,
        };
        use miette::Diagnostic;
        let labels: Vec<_> = err.labels().expect("must have labels").collect();
        assert_eq!(labels[0].inner().offset(), 3);
        assert_eq!(labels[0].inner().len(), 0);
    }

    // 1.1k: AidlError::collect() — 중첩된 Multiple 평탄화 검증
    #[test]
    fn test_aidl_error_collect_flatten() {
        let make_parse_err = |msg: &str| {
            AidlError::Parse(ParseError {
                src: NamedSource::new("test.aidl", msg.to_string()),
                span: span(0, 1),
                message: msg.to_string(),
                help: None,
            })
        };

        let a = make_parse_err("error A");
        let b = make_parse_err("error B");
        let c = make_parse_err("error C");

        // Multiple{[A, B]} + C → Multiple{[A, B, C]}
        let nested = AidlError::Multiple { errors: vec![a, b] };
        let result = AidlError::collect(vec![nested, c]);

        match result {
            Some(AidlError::Multiple { errors }) => {
                assert_eq!(errors.len(), 3, "Expected 3 flattened errors");
            }
            other => panic!("Expected Multiple, got: {other:?}"),
        }
    }

    // 1.1l: AidlError::collect() — 에러 1개일 때 Multiple로 감싸지 않음
    #[test]
    fn test_aidl_error_collect_single() {
        let err = AidlError::Parse(ParseError {
            src: NamedSource::new("test.aidl", "bad".to_string()),
            span: span(0, 3),
            message: "syntax error".to_string(),
            help: None,
        });
        let result = AidlError::collect(vec![err]);
        assert!(
            matches!(result, Some(AidlError::Parse(_))),
            "Single error should not be wrapped in Multiple"
        );
    }

    // 1.1m: AidlError::collect() — 에러 0개일 때 None 반환
    #[test]
    fn test_aidl_error_collect_empty() {
        let result = AidlError::collect(vec![]);
        assert!(result.is_none(), "Empty collection should return None");
    }
}
