fn main() {
    for path in std::env::args().skip(1) {
        let src = std::fs::read_to_string(&path).unwrap();
        let t0 = std::time::Instant::now();
        let doc = forge_sexpr::parse(&src).unwrap();
        let t_parse = t0.elapsed();
        let t1 = std::time::Instant::now();
        let board = galvani_extract::ExtractedBoard::from_kicad_pcb(&src).unwrap();
        let t_extract = t1.elapsed();
        let t2 = std::time::Instant::now();
        let emitted = doc.emit();
        let t_emit = t2.elapsed();
        assert_eq!(emitted.len(), src.len());
        println!("{path}: {} MB | parse {:?} | extract(total) {:?} | emit {:?} | {} comps {} nets",
            src.len()/1_000_000, t_parse, t_extract, t_emit, board.components.len(), board.nets.len());
    }
}
