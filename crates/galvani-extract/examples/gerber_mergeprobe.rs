use std::collections::{HashMap, BTreeSet};
use std::path::Path;
use galvani_extract::gerber::from_gerber_dir;
use galvani_extract::ExtractedBoard;
fn pad_key(x:f64,y:f64)->(i64,i64){((x*10.0).round() as i64,(y*10.0).round() as i64)}
fn main(){
    let a:Vec<String>=std::env::args().collect();
    let native=ExtractedBoard::from_kicad_pcb(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
    let recon=from_gerber_dir(Path::new(&a[2])).unwrap();
    // map pad pos -> native net name, recon net id
    let mut npos:HashMap<(i64,i64),String>=HashMap::new();
    for c in &native.components { for p in &c.pins { if let (Some((x,y)),Some(net))=(p.position,p.net){ if let Some(n)=native.net(net){ npos.insert(pad_key(x,-y), n.name.clone()); } } } }
    let mut rpos:HashMap<(i64,i64),i64>=HashMap::new();
    for c in &recon.board.components { for p in &c.pins { if let (Some((x,y)),Some(net))=(p.position,p.net){ rpos.insert(pad_key(x,y), net); } } }
    // group native net names by recon net id
    let mut g:HashMap<i64,BTreeSet<String>>=HashMap::new();
    for (k,rid) in &rpos { if let Some(nn)=npos.get(k){ g.entry(*rid).or_default().insert(nn.clone()); } }
    for (rid,names) in &g { if names.len()>1 { println!("recon NET {} merges native: {:?}", rid, names); } }
}
