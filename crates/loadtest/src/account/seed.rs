use anyhow::{Context as _, Result};

use rustibia_server::entities::vocation::Vocation;

use crate::account::rest::{Character, Session, SiteClient};

pub const MAX_BOT_INDEX: usize = 26 * 26 * 26 - 1;

/// Builds a name the site's `character_name::validate` will accept: letters and a
/// single space only. `index` becomes three base-26 digits, `a`-`z`, most significant
/// first, with the leading digit capitalised — `bot_name("Loadbot", 0) ==
/// "Loadbot Aaa"`, `bot_name("Loadbot", 1) == "Loadbot Aab"`.
pub fn bot_name(prefix: &str, index: usize) -> String {
    assert!(
        index <= MAX_BOT_INDEX,
        "bot index {index} exceeds the three-letter suffix ceiling of {MAX_BOT_INDEX}"
    );

    let mut digits = [0u8; 3];
    let mut remaining = index;
    for digit in digits.iter_mut().rev() {
        *digit = b'a' + (remaining % 26) as u8;
        remaining /= 26;
    }
    digits[0] = digits[0].to_ascii_uppercase();

    format!("{prefix} {}", std::str::from_utf8(&digits).unwrap())
}

pub async fn create_missing(
    site: &SiteClient,
    session: &Session,
    existing: &[Character],
    prefix: &str,
    count: usize,
    vocation: Vocation,
) -> Result<Vec<String>> {
    let mut created = Vec::new();

    for index in 0..count {
        let name = bot_name(prefix, index);
        if existing.iter().any(|c| c.name.eq_ignore_ascii_case(&name)) {
            continue;
        }

        site.create_character(session, &name, vocation)
            .await
            .with_context(|| format!("creating {name}"))?;
        created.push(name);
    }

    Ok(created)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::rest::{Character, CharacterId, Session};
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn only_the_missing_names_are_created() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/account/characters/new"))
            .and(body_string_contains("name=Loadbot+Aab"))
            .and(body_string_contains("vocation=2"))
            .respond_with(ResponseTemplate::new(303).insert_header("Location", "/account"))
            .expect(1)
            .mount(&server)
            .await;

        let existing = vec![Character {
            id: CharacterId(1),
            name: "Loadbot Aaa".to_string(),
        }];

        let created = create_missing(
            &SiteClient::new(&server.uri()).unwrap(),
            &Session {
                token: "sess-1".into(),
            },
            &existing,
            "Loadbot",
            2,
            Vocation::Sorcerer,
        )
        .await
        .unwrap();

        assert_eq!(created, vec!["Loadbot Aab".to_string()]);
    }

    #[tokio::test]
    async fn an_existing_name_is_matched_case_insensitively() {
        let server = MockServer::start().await;

        let existing = vec![Character {
            id: CharacterId(1),
            name: "loadbot aaa".to_string(),
        }];

        let created = create_missing(
            &SiteClient::new(&server.uri()).unwrap(),
            &Session {
                token: "sess-1".into(),
            },
            &existing,
            "Loadbot",
            1,
            Vocation::Sorcerer,
        )
        .await
        .unwrap();

        assert!(created.is_empty(), "must not recreate an existing name");
    }

    /// Mirrors `crates/site/src/domain/character_name.rs`'s `validate`: letters and a
    /// single interior space only, 2-29 characters, no leading, trailing or doubled
    /// space. `crates/loadtest` cannot link that crate to assert against it directly.
    #[test]
    fn generated_names_satisfy_the_sites_character_name_rule() {
        assert_eq!(bot_name("Loadbot", 0), "Loadbot Aaa");
        assert_eq!(bot_name("Loadbot", 1), "Loadbot Aab");
        assert_eq!(bot_name("Loadbot", 26), "Loadbot Aba");
        assert_eq!(bot_name("Loadbot", MAX_BOT_INDEX), "Loadbot Zzz");

        let mut seen = std::collections::HashSet::new();
        for index in [0, 1, 25, 26, MAX_BOT_INDEX] {
            let name = bot_name("Loadbot", index);

            assert!(
                name.chars().all(|c| c.is_alphabetic() || c == ' '),
                "{name} contains something other than letters and spaces"
            );
            assert!(
                (2..=29).contains(&name.chars().count()),
                "{name} is outside the site's length bounds"
            );
            assert!(
                !name.starts_with(' ') && !name.ends_with(' '),
                "{name} has leading or trailing whitespace"
            );
            assert!(!name.contains("  "), "{name} has a doubled space");
            assert!(seen.insert(name.to_lowercase()), "duplicate name: {name}");
        }
    }

    #[test]
    #[should_panic(expected = "exceeds the three-letter suffix ceiling")]
    fn an_index_past_the_ceiling_panics_rather_than_wrapping() {
        bot_name("Loadbot", MAX_BOT_INDEX + 1);
    }
}
