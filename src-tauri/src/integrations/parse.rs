#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedInput {
    MaybeVideoId(String),
    Query(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    InvalidInput,
    UnsupportedUrl,
    PlaylistNotSupported,
}

pub fn parse_input(input: &str) -> Result<ParsedInput, ParseError> {
    let s = input.trim();
    if s.is_empty() {
        return Err(ParseError::InvalidInput);
    }

    // If it looks like a URL
    if s.starts_with("http://") || s.starts_with("https://") {
        return parse_url(s);
    }

    // Bare token: 11 chars [A-Za-z0-9_-] => MaybeVideoId
    if is_likely_video_id(s) {
        return Ok(ParsedInput::MaybeVideoId(s.to_string()));
    }

    Ok(ParsedInput::Query(s.to_string()))
}

fn is_likely_video_id(s: &str) -> bool {
    if s.len() != 11 {
        return false;
    }
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn parse_url(s: &str) -> Result<ParsedInput, ParseError> {
    // Extract host
    let without_scheme =
        s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).unwrap_or(s);
    let host_end = without_scheme.find('/').unwrap_or(without_scheme.len());
    let host = &without_scheme[..host_end];
    let host_lc = host.to_ascii_lowercase();

    let allowed = matches!(
        host_lc.as_str(),
        "youtube.com" | "www.youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtu.be"
    );
    if !allowed {
        return Err(ParseError::UnsupportedUrl);
    }

    let path_and_query = &without_scheme[host_end..]; // starts with / or empty

    if host_lc == "youtu.be" {
        // /ID
        let path = path_and_query.trim_start_matches('/');
        let id = path.split(['?', '&', '/']).next().unwrap_or("").trim();
        if is_likely_video_id(id) {
            return Ok(ParsedInput::MaybeVideoId(id.to_string()));
        }
        return Err(ParseError::InvalidInput);
    }

    // Check for playlist-only: has list= but no v=
    let has_list = path_and_query.contains("list=");
    let has_v = path_and_query.contains("v=");

    if has_list && !has_v {
        // Could be playlist-only URL
        // But also check if it's a watch URL without v? unlikely
        if path_and_query.contains("/playlist") || has_list {
            return Err(ParseError::PlaylistNotSupported);
        }
    }

    // watch?v=
    if path_and_query.contains("/watch") && has_v {
        if let Some(pos) = path_and_query.find("v=") {
            let after = &path_and_query[pos + 2..];
            let id = after.split(['&', '#', '/']).next().unwrap_or("").trim();
            if is_likely_video_id(id) {
                return Ok(ParsedInput::MaybeVideoId(id.to_string()));
            }
        }
        return Err(ParseError::InvalidInput);
    }

    // /shorts/ID, /embed/ID, /live/ID
    for prefix in ["/shorts/", "/embed/", "/live/"] {
        if let Some(pos) = path_and_query.find(prefix) {
            let after = &path_and_query[pos + prefix.len()..];
            let id = after.split(['?', '&', '/']).next().unwrap_or("").trim();
            if is_likely_video_id(id) {
                return Ok(ParsedInput::MaybeVideoId(id.to_string()));
            }
        }
    }

    // If has v= somewhere else
    if has_v {
        if let Some(pos) = path_and_query.find("v=") {
            let after = &path_and_query[pos + 2..];
            let id = after.split(['&', '#', '/']).next().unwrap_or("").trim();
            if is_likely_video_id(id) {
                return Ok(ParsedInput::MaybeVideoId(id.to_string()));
            }
        }
    }

    Err(ParseError::InvalidInput)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bare_id() {
        assert_eq!(
            parse_input("dQw4w9WgXcQ").unwrap(),
            ParsedInput::MaybeVideoId("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn parse_youtube_watch() {
        assert_eq!(
            parse_input("https://www.youtube.com/watch?v=dQw4w9WgXcQ&t=30s").unwrap(),
            ParsedInput::MaybeVideoId("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn parse_music_youtube_with_list() {
        assert_eq!(
            parse_input("https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDCLAK5uy_").unwrap(),
            ParsedInput::MaybeVideoId("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn parse_youtu_be_si() {
        assert_eq!(
            parse_input("https://youtu.be/dQw4w9WgXcQ?si=tracking").unwrap(),
            ParsedInput::MaybeVideoId("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn parse_shorts() {
        assert_eq!(
            parse_input("https://www.youtube.com/shorts/dQw4w9WgXcQ").unwrap(),
            ParsedInput::MaybeVideoId("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn parse_embed() {
        assert_eq!(
            parse_input("https://www.youtube.com/embed/dQw4w9WgXcQ").unwrap(),
            ParsedInput::MaybeVideoId("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn parse_playlist_only() {
        assert_eq!(
            parse_input("https://music.youtube.com/playlist?list=PLxxx"),
            Err(ParseError::PlaylistNotSupported)
        );
    }

    #[test]
    fn parse_non_youtube_url() {
        assert_eq!(parse_input("https://vimeo.com/123456"), Err(ParseError::UnsupportedUrl));
    }

    #[test]
    fn parse_empty() {
        assert_eq!(parse_input(""), Err(ParseError::InvalidInput));
        assert_eq!(parse_input("   "), Err(ParseError::InvalidInput));
    }

    #[test]
    fn parse_query_words() {
        match parse_input("never gonna give you up").unwrap() {
            ParsedInput::Query(q) => assert_eq!(q, "never gonna give you up"),
            _ => panic!("expected query"),
        }
        // bohemianrhapsody is 15 chars, not 11 -> Query
        match parse_input("bohemianrhapsody").unwrap() {
            ParsedInput::Query(q) => assert_eq!(q, "bohemianrhapsody"),
            _ => panic!("expected query"),
        }
    }

    #[test]
    fn parse_photography_maybe_id() {
        // "photography" is 11 chars? let's check: p h o t o g r a p h y = 11 letters? count: photography = 11 chars? "photo"4 + "graphy"6 = 10? Or better test the actual string
        // The instruction says: "photography (MaybeVideoId)". Let me check: "photography" = p-h-o-t-o-g-r-a-p-h-y = 11 chars? "photograph" 10 + "y" = 11? "photography": p h o t o g r a p h y -> 11 letters: p,o,t,o,g,r,a,p,h,y plus h? Let's count: "photography" = "photo" (5) + "graphy" (6) = 11 chars total: "p","h","o","t","o","g","r","a","p","h","y" yes 11 chars.
        match parse_input("photography").unwrap() {
            ParsedInput::MaybeVideoId(s) => assert_eq!(s, "photography"),
            _ => panic!("expected maybe id"),
        }
    }
}
