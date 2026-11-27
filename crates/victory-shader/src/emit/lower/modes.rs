use std::cell::Cell;
use std::thread::LocalKey;

/// Restore the previous mode even when a worker catches a panic and
/// translates another shader on the same thread.
pub(super) fn with_lowering_mode<R>(mode: &'static LocalKey<Cell<bool>>, f: impl FnOnce() -> R) -> R {
    struct Restore {
        mode: &'static LocalKey<Cell<bool>>,
        previous: bool,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            self.mode.set(self.previous);
        }
    }
    let _restore = Restore {
        mode,
        previous: mode.replace(true),
    };
    f()
}

#[cfg(test)]
mod tests {
    #[test]
    fn scoped_lowering_modes_restore_after_a_panic() {
        use super::super::{
            FIRST_HALF_PAIRS, LOCAL_ANY, LOCAL_WAVES, LOOSE_LDS, with_first_half_pairs,
            with_local_waves, with_local_waves_any, with_loose_lds,
        };

        let modes = || (LOCAL_WAVES.get(), LOCAL_ANY.get(), FIRST_HALF_PAIRS.get(), LOOSE_LDS.get());
        assert_eq!(modes(), (false, false, false, false));

        with_local_waves(|| {
            with_local_waves_any(|| {
                with_first_half_pairs(|| assert_eq!(modes(), (true, true, true, false)));
                assert_eq!(modes(), (true, true, false, false));
            });
            assert_eq!(modes(), (true, false, false, false));
        });
        assert_eq!(modes(), (false, false, false, false));

        let failure = std::panic::catch_unwind(|| {
            with_local_waves_any(|| {
                with_first_half_pairs(|| {
                    with_loose_lds(|| {
                        assert_eq!(modes(), (true, true, true, true));
                        panic!("synthetic lowering panic");
                    });
                });
            });
        })
        .expect_err("the lowering panic should be caught");
        assert_eq!(failure.downcast_ref::<&str>(), Some(&"synthetic lowering panic"));
        assert_eq!(modes(), (false, false, false, false));
    }
}
