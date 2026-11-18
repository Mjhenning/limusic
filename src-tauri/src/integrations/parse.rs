use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedInput {
    VideoId(String),
    Query(String),
}

/// Extract a YouTube video id from common URL forms or treat as raw id/query.
/// Accepts:
/// - https://www.youtube.com/watch?v=VIDEOID
/// - https://music.youtube.com/watch?v=VIDEOID
/// - https://youtu.be/VIDEOID
/// - bare 11-char id (common case) OR any other non-URL string as Query
pub fn parse_input(input: &str) -> ParsedInput {
    let s = input.trim();
    if s.is_empty() {
        return ParsedInput::Query(String::new());
    }

    // Try URL parsing
    if let Some(id) = extract_video_id_from_url(s) {
        return ParsedInput::VideoId(id);
    }

    // Heuristic: if it looks like a video id (11 chars, alphanum + - _)
    if is_likely_video_id(s) {
        return ParsedInput::VideoId(s.to_string());
    }

    ParsedInput::Query(s.to_string())
}

fn extract_video_id_from_url(s: &str) -> Option<String> {
    // youtu.be/<id>
    if let Some(rest) = s.strip_prefix("https://youtu.be/") {
        let id = rest.split(['?', '&', '/']).next().unwrap_or("").trim();
        if is_likely_video_id(id) {
            return Some(id.to_string());
        }
    }
    if let Some(rest) = s.strip_prefix("http://youtu.be/") {
        let id = rest.split(['?', '&', '/']).next().unwrap_or("").trim();
        if is_likely_video_id(id) {
            return Some(id.to_string());
        }
    }
    // youtube.com or music.youtube.com watch?v=
    if s.contains("youtube.com/watch") || s.contains("music.youtube.com/watch") {
        // naive query parse
        if let Some(pos) = s.find("v=") {
            let after = &s[pos + 2..];
            let id = after.split(['&', '#', '/']).next().unwrap_or("").trim();
            if is_likely_video_id(id) {
                return Some(id.to_string());
            }
        }
    }
    None
}

fn is_likely_video_id(s: &str) -> bool {
    if s.len() != 11 {
        return false;
    }
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_video_id_bare() {
        assert_eq!(parse_input("dQw4w9WgXcQ"), ParsedInput::VideoId("dQw4w9WgXcQ".to_string()));
    }

    #[test]
    fn parse_youtube_url() {
        assert_eq!(
            parse_input("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            ParsedInput::VideoId("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn parse_music_youtube_url() {
        assert_eq!(
            parse_input("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RD"),
            ParsedInput::VideoId("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn parse_youtu_be() {
        assert_eq!(
            parse_input("https://youtu.be/dQw4w9WgXcQ?t=1"),
            ParsedInput::VideoId("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn parse_query() {
        match parse_input("hello world") {
            ParsedInput::Query(q) => assert_eq!(q, "hello world"),
            _ => panic!("expected query"),
        }
    }
}
