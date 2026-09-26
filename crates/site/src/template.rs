use askama::Template;
use axum::{
    http::StatusCode,
    response::{Html, IntoResponse, Response},
};

pub struct HtmlTemplate<T>(pub T);

impl<T: Template> IntoResponse for HtmlTemplate<T> {
    fn into_response(self) -> Response {
        match self.0.render() {
            Ok(body) => Html(body).into_response(),
            Err(err) => {
                tracing::error!("template render failed: {err}");
                (StatusCode::INTERNAL_SERVER_ERROR, "Template error").into_response()
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nav {
    None,
    News,
    Download,
    Rules,
    Support,
    Characters,
    Online,
    Highscores,
}

pub struct NavLink {
    pub href: &'static str,
    pub label: &'static str,
    pub current: bool,
}

impl Nav {
    pub fn game(self) -> Vec<NavLink> {
        self.links(&[
            (Nav::News, "/", "News"),
            (Nav::Download, "/download", "Download"),
            (Nav::Rules, "/rules", "Rules"),
            (Nav::Support, "/support", "Support"),
        ])
    }

    pub fn community(self) -> Vec<NavLink> {
        self.links(&[
            (Nav::Characters, "/characters", "Characters"),
            (Nav::Online, "/online", "Who Is Online"),
            (Nav::Highscores, "/highscores", "Highscores"),
        ])
    }

    fn links(self, entries: &[(Nav, &'static str, &'static str)]) -> Vec<NavLink> {
        entries
            .iter()
            .map(|&(nav, href, label)| NavLink {
                href,
                label,
                current: nav == self,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current(links: &[NavLink]) -> Vec<&'static str> {
        links.iter().filter(|l| l.current).map(|l| l.href).collect()
    }

    #[test]
    fn only_the_active_page_is_current() {
        assert_eq!(current(&Nav::Rules.game()), vec!["/rules"]);
        assert!(current(&Nav::Rules.community()).is_empty());
        assert_eq!(current(&Nav::Highscores.community()), vec!["/highscores"]);
    }

    #[test]
    fn a_page_outside_the_nav_marks_nothing() {
        assert!(current(&Nav::None.game()).is_empty());
        assert!(current(&Nav::None.community()).is_empty());
    }

    #[test]
    fn the_nav_lists_every_public_page_in_order() {
        let hrefs: Vec<_> = Nav::None
            .game()
            .into_iter()
            .chain(Nav::None.community())
            .map(|l| l.href)
            .collect();
        assert_eq!(
            hrefs,
            vec![
                "/",
                "/download",
                "/rules",
                "/support",
                "/characters",
                "/online",
                "/highscores"
            ]
        );
    }
}
