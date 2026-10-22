use super::*;

const STARTPAGE: &str = include_str!("fixtures/startpage.html");
const STARTPAGE_ANUBIS: &str = include_str!("fixtures/startpage_anubis.html");
const MOJEEK: &str = include_str!("fixtures/mojeek.html");
const MOJEEK_CAPTCHA: &str = include_str!("fixtures/mojeek_captcha.html");
const ECOSIA: &str = include_str!("fixtures/ecosia.html");
const ECOSIA_FIREWALL: &str = include_str!("fixtures/ecosia_firewall.html");

fn query(recency: Option<Recency>) -> Query<'static> {
    Query {
        text: "rust & async",
        limit: 3,
        recency,
        today: 19_723,
    }
}

#[test]
fn startpage_parses_titles_urls_and_snippets() {
    let results = parse(Provider::Startpage, STARTPAGE).unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].title, "Rust Programming Language");
    assert_eq!(results[0].url, "https://www.rust-lang.org/en-US");
    assert!(results[0].snippet.starts_with("Rust is blazingly fast"));
    assert!(results[0].snippet.ends_with('…') || results[0].snippet.ends_with("..."));
    assert_eq!(results[1].url, "https://rust.facepunch.com/");
    assert!(results[1].snippet.contains("island's wildlife"));
    assert_eq!(results[2].title, "Rust on Steam");
}

#[test]
fn mojeek_parses_titles_urls_and_snippets() {
    let results = parse(Provider::Mojeek, MOJEEK).unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].title, "Home - BBC News");
    assert_eq!(results[0].url, "https://www.bbc.co.uk/news");
    assert!(results[0].snippet.contains("independent investigation"));
    assert!(results[0].snippet.contains("news ..."));
    assert_eq!(
        results[1].title,
        "WordPress News – The latest news about WordPress and the"
    );
    assert!(results[2].snippet.contains("\" side-hustles \""));
}

#[test]
fn ecosia_parses_organic_results() {
    let results = parse(Provider::Ecosia, ECOSIA).unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].title, "Rust on Steam");
    assert_eq!(
        results[0].url,
        "https://store.steampowered.com/app/252490/Rust/"
    );
    assert!(results[0].snippet.starts_with("The only aim in Rust"));
    assert_eq!(results[2].url, "https://www.rust-lang.org/");
    assert!(results[2].snippet.starts_with("Rust is blazingly fast"));
}

#[test]
fn bot_walls_are_errors_not_empty_results() {
    let startpage = parse(Provider::Startpage, STARTPAGE_ANUBIS).unwrap_err();
    assert!(
        startpage.contains("Startpage") && startpage.contains("Anubis"),
        "{startpage}"
    );
    let mojeek = parse(Provider::Mojeek, MOJEEK_CAPTCHA).unwrap_err();
    assert!(
        mojeek.contains("Mojeek") && mojeek.contains("captcha"),
        "{mojeek}"
    );
    let ecosia = parse(Provider::Ecosia, ECOSIA_FIREWALL).unwrap_err();
    assert!(
        ecosia.contains("Ecosia") && ecosia.contains("firewall"),
        "{ecosia}"
    );
}

#[test]
fn unrecognised_layouts_fail_loudly_and_other_pages_are_not_cross_parsed() {
    let error = parse(
        Provider::Startpage,
        "<html><body><p>hello</p></body></html>",
    )
    .unwrap_err();
    assert!(error.contains("layout may have changed"), "{error}");
    assert!(parse(Provider::Startpage, MOJEEK).is_err());
    assert!(parse(Provider::Ecosia, STARTPAGE).is_err());
    assert!(parse(Provider::Brave, MOJEEK).is_err());
}

#[test]
fn mojeek_no_results_page_is_an_empty_success() {
    let page = "<html><body><div class=\"no-results\"><p>No pages found matching the query</p></div></body></html>";
    assert!(parse(Provider::Mojeek, page).unwrap().is_empty());
}

#[test]
fn requests_encode_the_query_and_map_startpage_recency() {
    let request = startpage(&query(Some(Recency::Week)));
    assert_eq!(request.method(), "GET");
    assert_eq!(
        request.url,
        "https://www.startpage.com/sp/search?query=rust%20%26%20async&cat=web&with_date=w"
    );
    assert!(!startpage(&query(None)).url.contains("with_date"));
    assert_eq!(
        mojeek(&query(None)).url,
        "https://www.mojeek.com/search?q=rust%20%26%20async"
    );
    assert_eq!(
        ecosia(&query(None)).url,
        "https://www.ecosia.org/search?q=rust%20%26%20async"
    );
    assert!(ecosia(&query(None)).headers.is_empty());
}
