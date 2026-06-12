use std::collections::{HashMap, BTreeSet};
use std::path::Path;
use galvani_extract::gerber::from_gerber_dir;
use galvani_extract::ExtractedBoard;
fn pad_key(x:f64,y:f64)->(i64,i64){((x*10.0).round() as i64,(y*10.0).round() as i64)}
fn main(){
    let a:Vec<String>=std::env::args().collect();
    let native=ExtractedBoard::from_kicad_pcb(&std::fs::read_to_string(&a[1]).unwrap()).unwrap();
    let recon=from_gerber_dir(Path::new(&a[2])).unwrap();
    let mut npos:HashMap<(i64,i64),String>=HashMap::new();
    for c in &native.components { for p in &c.pins { if let (Some((x,y)),Some(net))=(p.position,p.net){ if let Some(n)=native.net(net){ npos.insert(pad_key(x,-y), n.name.clone()); } } } }
    let mut rpos:HashMap<(i64,i64),i64>=HashMap::new();
    for c in &recon.board.components { for p in &c.pins { if let (Some((x,y)),Some(net))=(p.position,p.net){ rpos.insert(pad_key(x,y), net); } } }
    // For each native net, how many distinct recon nets do its pads land on?
    let mut by_native:HashMap<String,BTreeSet<i64>>=HashMap::new();
    let mut counts:HashMap<String,usize>=HashMap::new();
    for (k,nn) in &npos { if let Some(r)=rpos.get(k){ by_native.entry(nn.clone()).or_default().insert(*r); *counts.entry(nn.clone()).or_default()+=1; } }
    let mut v:Vec<_>=by_native.iter().filter(|(_,s)|s.len()>1).collect();
    v.sort_by_key(|(_,s)|std::cmp::Reverse(s.len()));
    for (nn,s) in v.iter().take(8){ println!("native {} ({} pads) SPLIT into {} recon nets", nn, counts[*nn], s.len()); }
}
