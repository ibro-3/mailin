//! The welcome document rendered inside WebView2.
//!
//! The real reader arrives in milestone 6 and will reuse the HTML policy from
//! the GTK front-end: a complete document with a restrictive CSP, no scripts,
//! and no remote resources. This page follows the same rules, but it is built
//! locally and never navigates anywhere.

use mailin_core::{Account, Error};

use crate::app::AppState;

/// Build the document shown when the spike starts.
pub fn document(state: &AppState) -> String {
    page(&render_accounts(state.mailin.list_accounts()))
}

/// Wrap a fragment in the complete document and its security policy.
fn page(accounts: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="Content-Security-Policy"
      content="default-src 'none'; style-src 'unsafe-inline'; img-src data:">
<title>Mailin</title>
<style>
  :root {{ color-scheme: light dark; }}
  body {{
    margin: 0;
    padding: 2.5rem 3rem;
    font-family: system-ui, "Segoe UI", sans-serif;
    line-height: 1.5;
    background: light-dark(#fafafa, #1e1e1e);
    color: light-dark(#1b1b1b, #f0f0f0);
  }}
  h1 {{ margin: 0 0 0.25rem; font-size: 2rem; font-weight: 600; }}
  p.lede {{ margin: 0 0 2rem; opacity: 0.75; }}
  h2 {{ margin: 2rem 0 0.75rem; font-size: 1.1rem; font-weight: 600; }}
  ul {{ margin: 0; padding: 0; list-style: none; }}
  li {{
    padding: 0.7rem 0.9rem;
    margin-bottom: 0.5rem;
    border: 1px solid light-dark(#dcdcdc, #3a3a3a);
    border-radius: 0.5rem;
    background: light-dark(#ffffff, #262626);
  }}
  li strong {{ display: block; font-weight: 600; }}
  li span {{ opacity: 0.7; font-size: 0.9rem; }}
  .empty {{ opacity: 0.7; font-style: italic; }}
  footer {{ margin-top: 2.5rem; font-size: 0.8rem; opacity: 0.55; }}
</style>
</head>
<body>
<h1>Mailin</h1>
<p class="lede">WinUI 3 binding spike — the same Rust core as the GTK client,
rendered by WebView2.</p>
<h2>Local accounts</h2>
{accounts}
<footer>Milestone 5 · projection, bootstrapping, window and WebView2</footer>
</body>
</html>
"#
    )
}

fn render_accounts(result: Result<Vec<Account>, Error>) -> String {
    match result {
        Ok(accounts) if accounts.is_empty() => {
            "<p class=\"empty\">No accounts yet. The account wizard lands with milestone 6.</p>"
                .to_owned()
        }
        Ok(accounts) => {
            let mut html = String::from("<ul>");
            for account in accounts {
                html.push_str("<li><strong>");
                html.push_str(&escape(&account.display_name));
                html.push_str("</strong><span>");
                html.push_str(&escape(&account.email));
                html.push_str("</span></li>");
            }
            html.push_str("</ul>");
            html
        }
        Err(error) => format!(
            "<p class=\"empty\">Could not read accounts: {}</p>",
            escape(&error.to_string())
        ),
    }
}

/// Escape text for HTML element content and quoted attributes.
fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            other => escaped.push(other),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_blocks_markup() {
        assert_eq!(
            escape("<img src=\"x\" onerror='y'>&"),
            "&lt;img src=&quot;x&quot; onerror=&#39;y&#39;&gt;&amp;"
        );
    }

    #[test]
    fn document_is_locked_down() {
        let html = page("");
        assert!(html.starts_with("<!doctype html>"));
        assert!(html.contains("default-src 'none'"));
        assert!(html.contains("img-src data:"));
        assert!(!html.contains("<script"));
    }

    #[test]
    fn accounts_are_escaped() {
        let account = Account::new(
            "<Ada>",
            "ada@example.com",
            mailin_core::ServerSettings::imap("imap.example.com", "ada"),
            mailin_core::ServerSettings::smtp("smtp.example.com", "ada"),
        );
        let html = render_accounts(Ok(vec![account]));
        assert!(html.contains("&lt;Ada&gt;"));
        assert!(!html.contains("<Ada>"));
    }
}
