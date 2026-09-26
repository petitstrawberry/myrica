//! One in-memory cookie jar shared by HTTP requests and the document's JS API.

use std::sync::Mutex;

use cookie_store::{Cookie, CookieDomain, CookieStore};
use reqwest::Url;
use reqwest::header::HeaderValue;

#[derive(Default)]
pub(super) struct BrowserCookies(Mutex<CookieStore>);

impl BrowserCookies {
    pub(super) fn document_cookies(&self, url: &Url) -> String {
        if !matches!(url.scheme(), "http" | "https") {
            return String::new();
        }
        let store = self.0.lock().unwrap();
        let mut cookies = store.matches(url);
        cookies.sort_by_key(|cookie| std::cmp::Reverse(cookie.path.as_ref().len()));
        cookies
            .into_iter()
            .filter(|cookie| !cookie.http_only().unwrap_or(false))
            .map(|cookie| format!("{}={}", cookie.name(), cookie.value()))
            .collect::<Vec<_>>()
            .join("; ")
    }

    pub(super) fn set_document_cookie(&self, url: &Url, value: &str) {
        self.store(url, value, false);
    }

    fn store(&self, url: &Url, value: &str, from_http: bool) {
        if !matches!(url.scheme(), "http" | "https") {
            return;
        }
        let Ok(mut cookie) = Cookie::parse(value.to_owned(), url) else {
            return;
        };
        if !from_http && cookie.http_only().unwrap_or(false) {
            return;
        }
        // Do not allow pages (or insecure HTTP responses) to plant Secure
        // cookies. In particular, honor the __Secure- and __Host- contracts.
        let secure_origin = url.scheme() == "https"
            || url.host_str() == Some("localhost")
            || url.host_str().is_some_and(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
            });
        let secure = cookie.secure().unwrap_or(false);
        if secure && !secure_origin {
            return;
        }
        if cookie.name().starts_with("__Secure-") && (!secure || !secure_origin) {
            return;
        }
        if cookie.name().starts_with("__Host-")
            && (!secure
                || !secure_origin
                || cookie.domain().is_some()
                || cookie.path() != Some("/"))
        {
            return;
        }
        // cookie_store checks domain matching; the static PSL also rejects
        // public-suffix Domain attributes (com, co.uk, github.io, ...).
        if let CookieDomain::Suffix(domain) = &cookie.domain {
            if psl::suffix(domain.as_bytes())
                .is_some_and(|suffix| suffix.as_bytes() == domain.as_bytes())
            {
                if url.host_str() != Some(domain.as_str()) {
                    return;
                }
                cookie.domain = CookieDomain::HostOnly(domain.clone());
            }
        }
        let Some(domain) = cookie.domain.as_cow() else {
            return;
        };
        let mut store = self.0.lock().unwrap();
        if !from_http
            && store
                .get(&domain, cookie.path.as_ref(), cookie.name())
                .is_some_and(|existing| existing.http_only().unwrap_or(false))
        {
            return;
        }
        let _ = store.insert(cookie, url);
    }
}

impl reqwest::cookie::CookieStore for BrowserCookies {
    fn set_cookies(&self, values: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        for value in values.filter_map(|value| value.to_str().ok()) {
            self.store(url, value, true);
        }
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        let value = self
            .0
            .lock()
            .unwrap()
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("; ");
        if value.is_empty() {
            None
        } else {
            HeaderValue::from_str(&value).ok()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::cookie::CookieStore as _;

    #[test]
    fn scripts_share_http_cookies_without_access_to_httponly() {
        let jar = BrowserCookies::default();
        let url = Url::parse("https://www.example.com/dir/page").unwrap();
        jar.set_cookies(
            &mut [HeaderValue::from_static(
                "secret=http; HttpOnly; Secure; Path=/",
            )]
            .iter(),
            &url,
        );
        jar.set_document_cookie(&url, "visible=script; Secure; Path=/");
        jar.set_document_cookie(&url, "secret=overwritten; Path=/");
        jar.set_document_cookie(&url, "secret=; Max-Age=0; Path=/");
        jar.set_document_cookie(&url, "forged=x; HttpOnly; Path=/");
        assert_eq!(jar.document_cookies(&url), "visible=script");
        let http = jar.cookies(&url).unwrap();
        let http = http.to_str().unwrap();
        assert!(http.contains("secret=http"));
        assert!(http.contains("visible=script"));
        assert!(!http.contains("forged"));
        jar.set_document_cookie(&url, "visible=; Max-Age=0; Path=/");
        assert_eq!(jar.document_cookies(&url), "");
    }

    #[test]
    fn cookie_scope_and_prefixes_are_enforced() {
        let jar = BrowserCookies::default();
        let https = Url::parse("https://www.example.com/dir/page").unwrap();
        jar.set_document_cookie(&https, "scoped=ok");
        jar.set_document_cookie(&https, "wrong=x; Domain=unrelated.test");
        jar.set_document_cookie(&https, "super=x; Domain=com");
        jar.set_document_cookie(
            &https,
            "__Host-invalid=x; Secure; Domain=example.com; Path=/",
        );
        jar.set_document_cookie(&https, "__Secure-invalid=x");
        assert_eq!(jar.document_cookies(&https), "scoped=ok");
        assert_eq!(
            jar.document_cookies(&Url::parse("https://www.example.com/elsewhere").unwrap()),
            ""
        );
        assert_eq!(
            jar.document_cookies(&Url::parse("https://other.example.com/dir/page").unwrap()),
            ""
        );
        let http = Url::parse("http://www.example.com/dir/page").unwrap();
        jar.set_document_cookie(&http, "insecure=x; Secure");
        jar.set_document_cookie(&https, "secure=ok; Secure");
        assert_eq!(jar.document_cookies(&http), "scoped=ok");
        assert!(!jar.document_cookies(&https).contains("insecure"));
    }
}
