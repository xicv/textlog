//! Apple Vision Framework OCR wrapper.
//!
//! Wraps `VNRecognizeTextRequest` + `VNImageRequestHandler` so the
//! pipeline (and the `textlog__ocr_image` MCP tool) can OCR clipboard
//! images and ad-hoc files entirely on-device.
//!
//! Threading: this module is *blocking* — Vision's `performRequests:`
//! is synchronous. Callers in async contexts must wrap with
//! `tokio::task::spawn_blocking`.

use crate::config::schema::OcrConfig;
use crate::error::{Error, Result};

/// Outcome of a single OCR pass.
#[derive(Debug, Clone)]
pub struct OcrResult {
    /// All recognized text from every block, joined with newlines.
    pub text: String,
    /// Mean confidence across all kept blocks (0.0 if no blocks).
    pub confidence: f32,
    /// Number of `VNRecognizedTextObservation`s above `min_confidence`.
    pub block_count: usize,
}

/// Run Apple Vision's `VNRecognizeTextRequest` against image bytes.
///
/// `bytes` may be any format CIImage accepts (PNG, JPEG, TIFF, HEIC, …).
/// Empty input is rejected up-front so we don't ship a degenerate
/// request to Vision.
pub fn ocr_image(bytes: &[u8], cfg: &OcrConfig) -> Result<OcrResult> {
    if bytes.is_empty() {
        return Err(Error::Ocr("ocr_image: empty input bytes".into()));
    }
    let level = parse_recognition_level(&cfg.recognition_level)?;
    run_vision(bytes, level, &cfg.languages, cfg.min_confidence)
}

/// Map the config string to the Vision enum. Anything other than
/// "accurate" or "fast" (case-insensitive) is rejected so silent
/// typos can't downgrade us to the wrong model.
pub(crate) fn parse_recognition_level(s: &str) -> Result<RecognitionLevel> {
    match s.to_ascii_lowercase().as_str() {
        "accurate" => Ok(RecognitionLevel::Accurate),
        "fast" => Ok(RecognitionLevel::Fast),
        other => Err(Error::Ocr(format!(
            "unknown ocr.recognition_level {other:?} (expected 'accurate' or 'fast')"
        ))),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RecognitionLevel {
    Accurate,
    Fast,
}

#[cfg(target_os = "macos")]
fn run_vision(
    bytes: &[u8],
    level: RecognitionLevel,
    languages: &[String],
    min_confidence: f32,
) -> Result<OcrResult> {
    use objc2::rc::{autoreleasepool, Retained};
    use objc2::AnyThread;
    use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
    use objc2_vision::{
        VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
    };

    autoreleasepool(|_| {
        let data: Retained<NSData> = NSData::with_bytes(bytes);
        let empty_options: Retained<NSDictionary<_, _>> = NSDictionary::new();

        let handler = VNImageRequestHandler::initWithData_options(
            VNImageRequestHandler::alloc(),
            &data,
            &empty_options,
        );

        let request: Retained<VNRecognizeTextRequest> = VNRecognizeTextRequest::new();

        let level_enum = match level {
            RecognitionLevel::Accurate => VNRequestTextRecognitionLevel::Accurate,
            RecognitionLevel::Fast => VNRequestTextRecognitionLevel::Fast,
        };
        request.setRecognitionLevel(level_enum);

        if !languages.is_empty() {
            let lang_strs: Vec<Retained<NSString>> = languages
                .iter()
                .map(|s| NSString::from_str(s))
                .collect();
            let lang_refs: Vec<&NSString> = lang_strs.iter().map(|r| r.as_ref()).collect();
            let lang_array = NSArray::from_slice(&lang_refs);
            request.setRecognitionLanguages(&lang_array);
        }

        let upcast: &VNRequest = request.as_ref();
        let requests = NSArray::from_slice(&[upcast]);

        handler
            .performRequests_error(&requests)
            .map_err(|nserr| Error::Ocr(format!("performRequests failed: {nserr}")))?;

        let observations = match request.results() {
            Some(arr) => arr,
            None => {
                return Ok(OcrResult {
                    text: String::new(),
                    confidence: 0.0,
                    block_count: 0,
                });
            }
        };

        let mut lines: Vec<String> = Vec::new();
        let mut confs: Vec<f32> = Vec::new();
        for obs in observations.iter() {
            let candidates = obs.topCandidates(1);
            if let Some(top) = candidates.iter().next() {
                let conf = top.confidence();
                if conf >= min_confidence {
                    lines.push(top.string().to_string());
                    confs.push(conf);
                }
            }
        }

        let mean = if confs.is_empty() {
            0.0
        } else {
            confs.iter().sum::<f32>() / confs.len() as f32
        };
        Ok(OcrResult {
            text: lines.join("\n"),
            confidence: mean,
            block_count: lines.len(),
        })
    })
}

#[cfg(not(target_os = "macos"))]
fn run_vision(
    _bytes: &[u8],
    _level: RecognitionLevel,
    _languages: &[String],
    _min_confidence: f32,
) -> Result<OcrResult> {
    Err(Error::Ocr(
        "OCR requires macOS (Apple Vision Framework)".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> OcrConfig {
        OcrConfig::default()
    }

    #[test]
    fn parse_recognition_level_accepts_accurate() {
        assert_eq!(
            parse_recognition_level("accurate").unwrap(),
            RecognitionLevel::Accurate
        );
    }

    #[test]
    fn parse_recognition_level_accepts_fast() {
        assert_eq!(parse_recognition_level("fast").unwrap(), RecognitionLevel::Fast);
    }

    #[test]
    fn parse_recognition_level_is_case_insensitive() {
        assert_eq!(
            parse_recognition_level("Accurate").unwrap(),
            RecognitionLevel::Accurate
        );
        assert_eq!(parse_recognition_level("FAST").unwrap(), RecognitionLevel::Fast);
    }

    #[test]
    fn parse_recognition_level_rejects_unknown() {
        let err = parse_recognition_level("turbo").unwrap_err();
        assert!(format!("{err}").contains("turbo"));
    }

    #[test]
    fn ocr_image_rejects_empty_bytes() {
        let err = ocr_image(&[], &cfg()).unwrap_err();
        assert!(format!("{err}").contains("empty input"));
    }

    #[test]
    fn ocr_image_rejects_invalid_recognition_level() {
        let mut c = cfg();
        c.recognition_level = "blazing".into();
        let err = ocr_image(b"\x89PNG\r\n\x1a\n", &c).unwrap_err();
        assert!(format!("{err}").contains("blazing"));
    }

    /// Real Apple Vision call against a 16×16 blank PNG. Asserts the
    /// FFI machinery completes and yields a sane (empty) result. Gated
    /// by `#[ignore]` so CI without an Apple Vision runtime is not
    /// surprised — run locally with `cargo test -- --ignored`.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires Apple Vision runtime; run with --ignored"]
    fn ocr_image_returns_empty_for_blank_png() {
        const BLANK_PNG: &[u8] = include_bytes!("../tests/fixtures/blank-16x16.png");
        let r = ocr_image(BLANK_PNG, &cfg()).expect("vision should not error on blank PNG");
        assert_eq!(r.text, "");
        assert_eq!(r.block_count, 0);
        assert_eq!(r.confidence, 0.0);
    }
}
