//! Timing probe against a local document; ignored by default.
//! Run with: PROBE_PDF=<path> cargo test -p pdf-editor-core --test perf_probe --release -- --ignored --nocapture
use pdf_editor_core::PdfEditor;
use pdf_editor_shared::FieldValue;
use std::time::Instant;

fn ms(t: Instant) -> u128 {
    t.elapsed().as_millis()
}

#[test]
#[ignore]
fn where_does_an_edit_go() {
    let bytes = std::fs::read(std::env::var("PROBE_PDF").expect("set PROBE_PDF")).unwrap();
    println!("document: {} KB", bytes.len() / 1024);

    let t = Instant::now();
    let mut ed = PdfEditor::new(&bytes).unwrap();
    println!("load                : {} ms", ms(t));

    let t = Instant::now();
    let fields = ed.form_fields().unwrap();
    println!("first form_fields   : {} ms ({} fields)", ms(t), fields.len());

    let t = Instant::now();
    let _ = ed.info().unwrap();
    println!("info (cached)       : {} ms", ms(t));

    let t = Instant::now();
    let _ = ed.render_page_png(0, 1.0).unwrap();
    println!("render page 0       : {} ms", ms(t));

    let target = fields
        .iter()
        .find(|f| !f.read_only && matches!(f.kind, pdf_editor_shared::FieldKind::Text { .. }))
        .expect("an editable text field");

    // A full edit cycle, as the UI performs it.
    let t_all = Instant::now();
    let t = Instant::now();
    ed.set_field_value(target.id, &FieldValue::Text("PERF".into())).unwrap();
    let set = ms(t);
    let t = Instant::now();
    let _ = ed.info().unwrap();
    let info = ms(t);
    let t = Instant::now();
    let _ = ed.form_fields().unwrap();
    let refresh = ms(t);
    let t = Instant::now();
    let _ = ed.render_page_png(0, 1.0).unwrap();
    let render = ms(t);
    println!("--- one edit cycle ---");
    println!("set_field_value     : {set} ms");
    println!("info()              : {info} ms   <- first call rebuilds the rasteriser");
    println!("form_fields()       : {refresh} ms");
    println!("render page 0       : {render} ms");
    println!("total               : {} ms", ms(t_all));
}
