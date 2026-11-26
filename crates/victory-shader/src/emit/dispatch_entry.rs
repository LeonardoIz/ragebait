//! Initial EXEC mask for a compute dispatch counted in threads.

use super::*;

impl Lower<'_> {
    pub(super) fn dispatch_entry_mask(&mut self) -> Result<(), Stop> {
        if self.x.st.entry_limited
            && let StageIo::Cs(cs) = &self.x.native.io
            && let Some(&(_, lo)) = self.x.p.inputs.iter().find(|i| i.0 == Reg::S(EXEC))
        {
            // Keep all lanes available for wave operations and barriers.
            // Only the initial EXEC bit depends on the dispatch's X count.
            let e = self.s.entry;
            let Val::I32(group) = self.compute_id("groupId.i32", 94, 0) else {
                unreachable!()
            };
            let Val::I32(thread) = self.thread_id(0) else {
                return Err(unsupported(
                    0,
                    "a thread id for the dispatch's thread count",
                ));
            };
            let size = self.s.m.i32(cs.threads[0]);
            let first = self.s.m.bin(e, Bin::Mul, group, size);
            let at = self.s.m.bin(e, Bin::Add, first, thread);
            let limit = self.host_word(RootType::Other(crate::emit::USER_THREAD_LIMIT), 0, 0);
            let zero = self.s.m.i32(0);
            let all = self.s.m.cmp(e, pred::EQ, limit, zero);
            let inside = self.s.m.cmp(e, pred::IULT, at, limit);
            let bit = self.s.m.bin(e, Bin::Or, all, inside);
            self.lane_bits.insert(lo, bit);
        }
        Ok(())
    }
}
