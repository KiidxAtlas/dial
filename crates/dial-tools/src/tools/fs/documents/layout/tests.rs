use super::*;

fn doc(style: &str, text: &str) -> Layout {
    let mut out = Layout::new("docx");
    parse(&mut out,"word/document.xml",&format!("<document><body><p><pPr><pStyle val='{style}'/><outlineLvl val='0'/></pPr><r><t>{text}</t></r></p><tbl><tr><tc><p><r><t>cell</t></r></p></tc></tr></tbl><sectPr><pgSz w='12240' h='15840'/><pgMar top='1440'/></sectPr></body></document>"),"document").unwrap();
    out
}
#[test]
fn native_structure_styles_and_page_declarations_remain_distinct_from_content() {
    let a = doc("Heading1", "Title");
    let b = doc("Caption", "Title");
    assert_eq!(a.content(), b.content());
    assert_ne!(a.structure(), b.structure());
    assert_eq!(
        a.nodes[0].attributes["heading_basis"],
        "explicit outline level"
    );
    assert!(b.nodes[0].attributes["role_basis"].contains("heuristic"));
    for kind in ["tbl", "tr", "tc", "pgSz", "pgMar"] {
        assert!(a.nodes.iter().any(|n| n.kind == kind), "missing {kind}");
    }
    assert_eq!(a.structure(), doc("Heading1", "Changed").structure());
    assert_ne!(a.content(), doc("Heading1", "Changed").content());
}
#[test]
fn pptx_geometry_is_preserved_without_claiming_visual_order() {
    let mut out = Layout::new("pptx");
    parse(&mut out,"ppt/slides/slide1.xml","<sld><sp><nvSpPr><cNvPr id='2' name='Title'/><ph type='title'/></nvSpPr><spPr><xfrm><off x='12' y='34'/><ext cx='56' cy='78'/></xfrm></spPr><txBody><p><r><t>Hello</t></r></p></txBody></sp></sld>","sld").unwrap();
    assert_eq!(out.content(), vec!["Hello"]);
    assert_eq!(
        out.nodes
            .iter()
            .find(|n| n.kind == "off")
            .unwrap()
            .attributes["x"],
        "12"
    );
    assert!(out.reading_order.contains("not guaranteed"));
}
#[test]
fn pdf_is_explicitly_heuristic_and_empty_scanned_text_is_not_semantic_layout() {
    let out = pdf_text("Hello\n\x0c\x0c").unwrap();
    assert_eq!(
        out.nodes
            .iter()
            .filter(|n| n.kind == "page_heuristic")
            .count(),
        2
    );
    assert!(out.limitations.iter().any(|s| s.contains("no OCR")));
    assert!(!out.nodes.iter().any(|n| n.kind == "heading"));
}
#[test]
fn xml_rejects_entities_and_malformed_roots() {
    let mut out = Layout::new("docx");
    assert!(
        parse(
            &mut out,
            "x",
            "<!DOCTYPE document [<!ENTITY x SYSTEM 'file:///secret'>]><document/>",
            "document"
        )
        .is_err()
    );
    assert!(parse(&mut out, "x", "<document><p></document>", "document").is_err());
}
