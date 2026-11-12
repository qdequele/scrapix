//! Where sitemap discovery looks, and what it accepts as a sitemap.

use scrapix_crawler::{SitemapConfig, SitemapParser};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn base(server: &MockServer) -> String {
    server.uri().replace("127.0.0.1", "localhost")
}

fn parser() -> SitemapParser {
    SitemapParser::new(SitemapConfig {
        allow_private_ips: true, // wiremock listens on 127.0.0.1
        ..Default::default()
    })
}

fn urlset(urls: &[String]) -> String {
    let entries: String = urls
        .iter()
        .map(|u| format!("<url><loc>{u}</loc></url>"))
        .collect();
    format!(
        r#"<?xml version="1.0"?><urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">{entries}</urlset>"#
    )
}

async fn serve(server: &MockServer, at: &str, body: String) {
    Mock::given(method("GET"))
        .and(path(at))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

fn locs(urls: Vec<scrapix_crawler::SitemapUrl>) -> Vec<String> {
    urls.into_iter().map(|u| u.loc).collect()
}

/// No robots.txt and no `/sitemap.xml`: the other usual names are tried.
#[tokio::test]
async fn finds_a_sitemap_at_another_usual_name() {
    let server = MockServer::start().await;
    let root = base(&server);
    serve(
        &server,
        "/sitemap_index.xml",
        urlset(&[format!("{root}/a")]),
    )
    .await;

    let found = parser().discover(&root).await.unwrap();
    assert_eq!(found.sitemaps, vec![format!("{root}/sitemap_index.xml")]);
    assert_eq!(locs(found.urls), vec![format!("{root}/a")]);
}

/// A sitemap declared in robots.txt that does not exist does not stop
/// discovery.
#[tokio::test]
async fn falls_back_when_the_declared_sitemap_is_broken() {
    let server = MockServer::start().await;
    let root = base(&server);
    serve(
        &server,
        "/robots.txt",
        format!("User-agent: *\nSitemap: {root}/gone.xml\n"),
    )
    .await;
    serve(&server, "/sitemap.xml", urlset(&[format!("{root}/a")])).await;

    let found = parser().discover(&root).await.unwrap();
    assert_eq!(found.sitemaps, vec![format!("{root}/sitemap.xml")]);
    assert_eq!(locs(found.urls), vec![format!("{root}/a")]);
}

/// A declared sitemap that works is enough: the usual names are not
/// requested.
#[tokio::test]
async fn declared_sitemaps_are_not_second_guessed() {
    let server = MockServer::start().await;
    let root = base(&server);
    serve(
        &server,
        "/robots.txt",
        format!("Sitemap: {root}/custom.xml\n"),
    )
    .await;
    serve(&server, "/custom.xml", urlset(&[format!("{root}/a")])).await;
    Mock::given(method("GET"))
        .and(path("/sitemap.xml"))
        .respond_with(ResponseTemplate::new(200).set_body_string(urlset(&[format!("{root}/b")])))
        .expect(0)
        .mount(&server)
        .await;

    let found = parser().discover(&root).await.unwrap();
    assert_eq!(locs(found.urls), vec![format!("{root}/a")]);
}

/// A site that serves its HTML 404 page with a 200 on `/sitemap.xml`.
#[tokio::test]
async fn an_html_page_is_not_a_sitemap() {
    let server = MockServer::start().await;
    let root = base(&server);
    serve(
        &server,
        "/sitemap.xml",
        format!(r#"<!DOCTYPE html><html><body><a href="{root}/x">x</a></body></html>"#),
    )
    .await;
    serve(&server, "/wp-sitemap.xml", urlset(&[format!("{root}/a")])).await;

    let found = parser().discover(&root).await.unwrap();
    assert_eq!(found.sitemaps, vec![format!("{root}/wp-sitemap.xml")]);
    assert_eq!(locs(found.urls), vec![format!("{root}/a")]);
}

/// A start URL inside the site also reads the section's own sitemap,
/// before the site-wide ones.
#[tokio::test]
async fn reads_the_sitemap_of_the_start_urls_folder() {
    let server = MockServer::start().await;
    let root = base(&server);
    serve(&server, "/sitemap.xml", urlset(&[format!("{root}/home")])).await;
    serve(
        &server,
        "/docs/sitemap.xml",
        urlset(&[format!("{root}/docs/a")]),
    )
    .await;

    let found = parser()
        .discover(&format!("{root}/docs/guide/intro.html"))
        .await
        .unwrap();
    assert_eq!(
        locs(found.urls),
        vec![format!("{root}/docs/a"), format!("{root}/home")]
    );
}

/// The same URL listed by two sitemaps comes out once.
#[tokio::test]
async fn urls_are_deduplicated_across_sitemaps() {
    let server = MockServer::start().await;
    let root = base(&server);
    let urls = [format!("{root}/a"), format!("{root}/b")];
    serve(&server, "/sitemap.xml", urlset(&urls)).await;
    serve(&server, "/sitemap_index.xml", urlset(&urls)).await;

    let found = parser().discover(&root).await.unwrap();
    assert_eq!(locs(found.urls), urls.to_vec());
    assert_eq!(found.sitemaps, vec![format!("{root}/sitemap.xml")]);
}

#[tokio::test]
async fn reads_text_rss_and_atom_sitemaps() {
    let server = MockServer::start().await;
    let root = base(&server);
    serve(
        &server,
        "/robots.txt",
        format!("Sitemap: {root}/urls.txt\nsitemap: {root}/rss.xml\nSITEMAP: /atom.xml\n"),
    )
    .await;
    serve(&server, "/urls.txt", format!("{root}/t1\n\n{root}/t2\n")).await;
    serve(
        &server,
        "/rss.xml",
        format!(
            r#"<?xml version="1.0"?><rss version="2.0"><channel><link>{root}/</link>
            <item><title>One</title><link>{root}/r1</link><pubDate>Mon, 15 Jan 2024 10:00:00 GMT</pubDate></item>
            <item><link><![CDATA[{root}/r2]]></link></item></channel></rss>"#
        ),
    )
    .await;
    serve(
        &server,
        "/atom.xml",
        format!(
            r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom"><link href="{root}/"/>
            <entry><link rel="self" href="{root}/self"/><link href="{root}/a1"/><updated>2024-01-15T10:00:00Z</updated></entry>
            <entry><link rel="alternate" href="{root}/a2"></link></entry></feed>"#
        ),
    )
    .await;

    let found = parser().discover(&root).await.unwrap();
    assert_eq!(found.sitemaps.len(), 3);
    let rss_first = found.urls.iter().find(|u| u.loc.ends_with("/r1")).unwrap();
    assert!(rss_first.lastmod.is_some());
    let atom_first = found.urls.iter().find(|u| u.loc.ends_with("/a1")).unwrap();
    assert!(atom_first.lastmod.is_some());
    assert_eq!(
        locs(found.urls),
        ["t1", "t2", "r1", "r2", "a1", "a2"].map(|p| format!("{root}/{p}"))
    );
}

/// robots.txt redirecting to the `www.` host tells where the site lives:
/// the usual locations are tried there.
#[tokio::test]
async fn follows_the_site_to_its_www_host() {
    let server = MockServer::start().await;
    let root = base(&server);
    let port = server.address().port();
    let bare = format!("localhost:{port}");
    let www = format!("http://www.localhost:{port}");
    Mock::given(method("GET"))
        .and(path("/robots.txt"))
        .and(header("host", bare.as_str()))
        .respond_with(
            ResponseTemplate::new(301).insert_header("location", format!("{www}/robots.txt")),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/sitemap.xml"))
        .and(header("host", format!("www.localhost:{port}").as_str()))
        .respond_with(ResponseTemplate::new(200).set_body_string(urlset(&[format!("{www}/a")])))
        .mount(&server)
        .await;

    let found = parser().discover(&root).await.unwrap();
    assert_eq!(found.origin, www);
    assert_eq!(found.sitemaps, vec![format!("{www}/sitemap.xml")]);
    assert_eq!(locs(found.urls), vec![format!("{www}/a")]);
}

/// Nothing anywhere: an empty result, not an error.
#[tokio::test]
async fn a_site_without_sitemap_gives_an_empty_result() {
    let server = MockServer::start().await;
    let found = parser().discover(&base(&server)).await.unwrap();
    assert!(found.urls.is_empty());
    assert!(found.sitemaps.is_empty());
}

#[tokio::test]
async fn raw_ip_start_urls_are_refused() {
    assert!(parser().discover("http://127.0.0.1:9/").await.is_err());
}
