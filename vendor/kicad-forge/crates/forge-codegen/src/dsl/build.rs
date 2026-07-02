//! Interpreter: [`Program`] -> [`forge_model::Pcb`].
//!
//! Executes the program by walking the body, declaring nets (in declaration
//! order, plus any net first seen on a pad), and emitting one footprint per
//! component with its full pad geometry and net assignments. The result is a
//! valid `.kicad_pcb` that re-opens in KiCad and is connectivity-equivalent to
//! the source board.

use crate::dsl::model::{Comp, Program, Stmt};
use forge_model::{FootprintBuilder, Pcb, PcbBuilder};
use std::collections::HashMap;

impl Program {
    /// Interpret the program into a [`Pcb`].
    pub fn build(&self) -> Pcb {
        // Net id assignment: declared nets first (in order), then any net that
        // only appears on a pad. Id 0 is reserved for unconnected by the
        // builder.
        let mut net_id: HashMap<String, i64> = HashMap::new();
        let mut next: i64 = 1;
        let mut order: Vec<(i64, String)> = Vec::new();
        let intern = |name: &str,
                          net_id: &mut HashMap<String, i64>,
                          next: &mut i64,
                          order: &mut Vec<(i64, String)>|
         -> i64 {
            if let Some(&id) = net_id.get(name) {
                return id;
            }
            let id = *next;
            *next += 1;
            net_id.insert(name.to_string(), id);
            order.push((id, name.to_string()));
            id
        };

        for st in &self.body {
            if let Stmt::Net(n) = st {
                intern(n, &mut net_id, &mut next, &mut order);
            }
        }
        // Ensure every pad net is interned (auto-declare).
        for c in self.comps() {
            for p in &c.pads {
                if let Some(n) = &p.net {
                    intern(n, &mut net_id, &mut next, &mut order);
                }
            }
        }

        let mut builder = PcbBuilder::new(self.version).standard_2layer_layers();
        for (id, name) in &order {
            builder = builder.add_net(*id, name);
        }

        for c in self.comps() {
            builder = builder.add_footprint(build_footprint(c, &net_id));
        }

        // Emit the board outline as four Edge.Cuts segments so the rebuilt board
        // carries a real boundary: KiCad draws it, and freerouting needs it as
        // the routing keep-in. The outline is the rectangle the placer honoured.
        if let Some(o) = &self.outline {
            let w = 0.1; // Edge.Cuts line width (mm), cosmetic.
            let corners = [
                (o.min_x, o.min_y),
                (o.max_x, o.min_y),
                (o.max_x, o.max_y),
                (o.min_x, o.max_y),
            ];
            for i in 0..4 {
                let a = corners[i];
                let b = corners[(i + 1) % 4];
                builder = builder.add_gr_line(a, b, w, "Edge.Cuts");
            }
        }

        builder.build()
    }
}

fn build_footprint(c: &Comp, net_id: &HashMap<String, i64>) -> FootprintBuilder {
    let mut fb = FootprintBuilder::new(&c.lib_id, &c.reference, &c.value)
        .at(c.at.0, c.at.1, c.rot)
        .layer(&c.layer);
    for p in &c.pads {
        let net = p.net.as_ref().map(|n| (net_id[n], n.as_str()));
        let layers: Vec<&str> = p.layers.iter().map(|s| s.as_str()).collect();
        fb = fb.add_pad(
            &p.number,
            &p.kind,
            &p.shape,
            p.at,
            p.size,
            p.drill,
            layers,
            net,
        );
    }
    fb
}
