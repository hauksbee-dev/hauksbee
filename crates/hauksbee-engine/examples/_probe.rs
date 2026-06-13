use hauksbee_ir::{Circuit, Device, NodeId, SourceKind};
use hauksbee_solve::{SolverOptions, StepControl, Transient, Partitioning, Integration};
const V_REF:f64=2.5; const C_MEM:f64=10e-9; const R_LEAK:f64=120e3; const TAU:f64=R_LEAK*C_MEM;
const I_UNIT:f64=(5.0-0.65)/10e6;
const THETA0:f64={let gt=1.0/820e3; let gb=1.0/150e3; (5.0*gt+2.5*gb)/(gt+gb)-2.5};
fn sopts()->SolverOptions{let mut o=SolverOptions::default(); o.integration=Integration::Trapezoidal;
    o.step=StepControl::Fixed{dt:2e-6}; o.partitioning=Partitioning::Off; o}
fn cross(tm:&[f64],v:&[f64],tg:f64)->Option<f64>{for i in 1..v.len(){let(a,b)=(v[i-1],v[i]);
    if a<tg&&b>=tg{let f=(tg-a)/(b-a);return Some(tm[i-1]+f*(tm[i]-tm[i-1]));}}None}
fn membrane(units:f64)->(Circuit,){
    let mut c=Circuit::new(); let vref=c.node("VREF"); let mem=c.node("MEM");
    c.add(Device::Vsource{name:"Vref".into(),p:vref,n:NodeId::GROUND,kind:SourceKind::Dc(V_REF)});
    c.add(Device::Resistor{name:"Rl".into(),a:mem,b:vref,ohms:R_LEAK,tc1:None});
    c.add(Device::Capacitor{name:"Cm".into(),a:mem,b:vref,farads:C_MEM,ic:Some(0.0)});
    c.add(Device::Isource{name:"Isyn".into(),p:NodeId::GROUND,n:mem,kind:SourceKind::Dc(units*I_UNIT)});
    (c,)
}
fn main(){
    println!("THETA0={:.4}V I_UNIT={:.4}uA TAU={:.4}ms",THETA0,I_UNIT*1e6,TAU*1e3);
    // TAU test: units=4
    {let (c,)=membrane(4.0); let wf=Transient::new(sopts()).run(&c,6.0*TAU).unwrap();
     let v=wf.node(&c,"MEM").unwrap(); let v0=v[0]; let vinf=*v.last().unwrap(); let span=vinf-v0;
     let tau=cross(&wf.time,v,v0+0.632*span);
     println!("TAU(u=4): v0={:.4} vinf={:.4} span={:.4} pred_span={:.4} tau={:.4}ms",
        v0,vinf,span,R_LEAK*4.0*I_UNIT,tau.unwrap_or(0.)*1e3);}
    // Threshold+spike: units=12
    {let (mut c,)=membrane(12.0); let mem=c.node("MEM"); let thr=c.node("THR"); let sp=c.node("SPIKE");
     c.add(Device::Vsource{name:"Vthr".into(),p:thr,n:NodeId::GROUND,kind:SourceKind::Dc(V_REF+THETA0)});
     c.add(Device::Comparator{name:"U".into(),out:sp,inp:mem,inn:thr,out_lo:0.0,out_hi:5.0,hysteresis:0.003});
     let span=R_LEAK*12.0*I_UNIT; let pred=-TAU*(1.0-THETA0/span).ln();
     let wf=Transient::new(sopts()).run(&c,(pred*2.5).max(4.0*TAU)).unwrap();
     let vm=wf.node(&c,"MEM").unwrap(); let vs=wf.node(&c,"SPIKE").unwrap();
     let tc=cross(&wf.time,vm,V_REF+THETA0); let ts=cross(&wf.time,vs,2.5);
     println!("THRESH(u=12): pred_cross={:.4}ms actual={:.4}ms span={:.4} spike_max={:.3} t_spike={:.4}ms",
        pred*1e3, tc.unwrap_or(0.)*1e3, span, vs.iter().cloned().fold(f64::MIN,f64::max), ts.unwrap_or(0.)*1e3);}
}
