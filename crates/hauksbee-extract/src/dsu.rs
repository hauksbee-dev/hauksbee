//! Disjoint-set forest (union-find) with path halving and union by size. The
//! one partition structure behind copper connectivity, schematic nets and X2
//! net-tie groups: every reader that merges elements into nets unions here and
//! reads the final partition back with `find`, so the merge rules live in the
//! readers and the bookkeeping lives in one place.

pub(crate) struct Dsu {
    parent: Vec<usize>,
    size: Vec<usize>,
}

impl Dsu {
    /// `n` singleton sets.
    pub(crate) fn new(n: usize) -> Self {
        Dsu {
            parent: (0..n).collect(),
            size: vec![1; n],
        }
    }

    /// A fresh singleton; its id.
    pub(crate) fn make(&mut self) -> usize {
        self.parent.push(self.parent.len());
        self.size.push(1);
        self.parent.len() - 1
    }

    pub(crate) fn find(&mut self, mut x: usize) -> usize {
        while self.parent[x] != x {
            self.parent[x] = self.parent[self.parent[x]];
            x = self.parent[x];
        }
        x
    }

    pub(crate) fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra == rb {
            return;
        }
        let (big, small) = if self.size[ra] >= self.size[rb] {
            (ra, rb)
        } else {
            (rb, ra)
        };
        self.parent[small] = big;
        self.size[big] += self.size[small];
    }
}
