//! Experimental support for vanguards.
//!
//! For more information, see the [vanguards spec].
//!
//! [vanguards spec]: https://spec.torproject.org/vanguards-spec/index.html.

pub mod config;
mod err;
mod set;

use std::sync::{Arc, RwLock, Weak};
use std::time::{Duration, SystemTime};

use futures::stream::BoxStream;
use futures::task::SpawnExt as _;
use futures::{future, FutureExt as _};
use futures::{select_biased, StreamExt as _};
use postage::stream::Stream as _;
use postage::watch;
use rand::RngCore;

use tor_async_utils::PostageWatchSenderExt as _;
use tor_config::ReconfigureError;
use tor_error::{error_report, internal, into_internal};
use tor_netdir::{DirEvent, NetDir, NetDirProvider, Timeliness};
use tor_persist::{DynStorageHandle, StateMgr};
use tor_relay_selection::RelaySelector;
use tor_rtcompat::Runtime;
use tracing::{debug, info};

use crate::{RetireCircuits, VanguardMode};

use set::VanguardSets;

use crate::VanguardConfig;
pub use config::VanguardParams;
pub use err::VanguardMgrError;
pub use set::Vanguard;

/// The key used for storing the vanguard sets to persistent storage using `StateMgr`.
const STORAGE_KEY: &str = "vanguards";

/// The vanguard manager.
pub struct VanguardMgr<R: Runtime> {
    /// The mutable state.
    inner: RwLock<Inner>,
    /// The runtime.
    runtime: R,
    /// The persistent storage handle, used for writing the vanguard sets to disk
    /// if full vanguards are enabled.
    storage: DynStorageHandle<VanguardSets>,
}

/// The mutable inner state of [`VanguardMgr`].
struct Inner {
    /// The current vanguard parameters.
    params: VanguardParams,
    /// Whether to use full, lite, or no vanguards.
    ///
    // TODO(#1382): we should derive the mode from the
    // vanguards-enabled and vanguards-hs-service consensus params.
    mode: VanguardMode,
    /// The L2 and L3 vanguards.
    ///
    /// The L3 vanguards are only used if we are running in
    /// [`Full`](VanguardMode::Full) vanguard mode.
    /// Otherwise, the L3 set is not populated, or read from.
    ///
    /// If [`Full`](VanguardMode::Full) vanguard mode is enabled,
    /// the vanguard sets will be persisted to disk whenever
    /// vanuards are rotated, added, or removed.
    ///
    /// The vanguard sets are updated and persisted to storage by
    /// [`update_vanguard_sets`](Inner::update_vanguard_sets).
    ///
    /// If the `VanguardSets` change while we are in "lite" mode,
    /// the changes will not *not* be written to storage.
    /// If we later switch to "full" vanguards, those previous changes still
    /// won't be persisted to storage: we only flush to storage if the
    /// [`VanguardSets`] change *while* we are in "full" mode
    /// (changing the [`VanguardMode`] does not constitute a change in the `VanguardSets`).
    //
    // TODO HS-VANGUARDS: the correct behaviour here might be to never switch back to lite mode
    // after enabling full vanguards. If we do that, persisting the vanguard sets will be simpler,
    // as we can just unconditionally flush to storage if the vanguard mode is switched to full.
    // Right now we can't do that, because we don't remember the "mode":
    // we derive it on the fly from `has_onion_svc` and the current `VanguardParams`.
    //
    ///
    /// This is initialized with the vanguard sets read from the vanguard state file,
    /// if the file exists, or with a [`Default`] `VanguardSets`, if it doesn't.
    ///
    /// Note: The `VanguardSets` are read from the vanguard state file
    /// even if full vanguards are not enabled. They are *not*, however, written
    /// to the state file unless full vanguards are in use.
    vanguard_sets: VanguardSets,
    /// Whether we're running an onion service.
    ///
    // TODO(#1382): This should be used for deciding whether to use the `vanguards_hs_service` or the
    // `vanguards_enabled` [`NetParameter`](tor_netdir::params::NetParameters).
    #[allow(unused)]
    has_onion_svc: bool,
    /// A channel for sending VanguardConfig changes to the vanguard maintenance task.
    config_tx: watch::Sender<VanguardConfig>,
}

/// Whether the [`VanguardMgr::maintain_vanguard_sets`] task
/// should continue running or shut down.
///
/// Returned from [`VanguardMgr::run_once`].
#[derive(Copy, Clone, Debug)]
enum ShutdownStatus {
    /// Continue calling `run_once`.
    Continue,
    /// The `VanguardMgr` was dropped, terminate the task.
    Terminate,
}

impl<R: Runtime> VanguardMgr<R> {
    /// Create a new `VanguardMgr`.
    ///
    /// The `state_mgr` handle is used for persisting the "vanguards-full" guard pools to disk.
    pub fn new<S>(
        config: &VanguardConfig,
        runtime: R,
        state_mgr: S,
        has_onion_svc: bool,
    ) -> Result<Self, VanguardMgrError>
    where
        S: StateMgr + Send + Sync + 'static,
    {
        // Note: we start out with default vanguard params, but we adjust them
        // as soon as we obtain a NetDir (see Self::run_once()).
        let params = VanguardParams::default();
        let storage: DynStorageHandle<VanguardSets> = state_mgr.create_handle(STORAGE_KEY);

        let vanguard_sets = match storage.load()? {
            Some(mut sets) => {
                info!("Loading vanguards from vanguard state file");
                // Discard the now-expired the vanguards
                let now = runtime.wallclock();
                let _ = sets.remove_expired(now);
                sets
            }
            None => {
                debug!("Vanguard state file not found, selecting new vanguards");
                // Initially, all sets have a target size of 0.
                // This is OK because the target is only used for repopulating the vanguard sets,
                // and we can't repopulate the sets without a netdir.
                // The target gets adjusted once we obtain a netdir.
                Default::default()
            }
        };

        let (config_tx, _config_rx) = watch::channel();
        let inner = Inner {
            params,
            mode: config.mode(),
            vanguard_sets,
            has_onion_svc,
            config_tx,
        };

        Ok(Self {
            inner: RwLock::new(inner),
            runtime,
            storage,
        })
    }

    /// Launch the vanguard pool management tasks.
    ///
    /// These run until the `VanguardMgr` is dropped.
    //
    // This spawns [`VanguardMgr::maintain_vanguard_sets`].
    pub fn launch_background_tasks(
        self: &Arc<Self>,
        netdir_provider: &Arc<dyn NetDirProvider>,
    ) -> Result<(), VanguardMgrError>
    where
        R: Runtime,
    {
        let netdir_provider = Arc::clone(netdir_provider);
        let config_rx = self
            .inner
            .write()
            .expect("poisoned lock")
            .config_tx
            .subscribe();
        self.runtime
            .spawn(Self::maintain_vanguard_sets(
                Arc::downgrade(self),
                Arc::downgrade(&netdir_provider),
                config_rx,
            ))
            .map_err(|e| VanguardMgrError::Spawn(Arc::new(e)))?;

        Ok(())
    }

    /// Replace the configuration in this `VanguardMgr` with the specified `config`.
    pub fn reconfigure(&self, config: &VanguardConfig) -> Result<RetireCircuits, ReconfigureError> {
        // TODO(#1382): abolish VanguardConfig and derive the mode from the VanguardParams
        // and has_onion_svc instead.
        //
        // TODO(#1382): update has_onion_svc if the new config enables onion svc usage
        //
        // Perhaps we should always escalate to Full if we start running an onion service,
        // but not decessarily downgrade to lite if we stop.
        // See <https://gitlab.torproject.org/tpo/core/arti/-/merge_requests/2083#note_3018173>
        let mut inner = self.inner.write().expect("poisoned lock");
        let new_mode = config.mode();
        if new_mode != inner.mode {
            inner.mode = new_mode;

            // Wake up the maintenance task to replenish the vanguard pools.
            inner.config_tx.maybe_send(|_| config.clone());

            Ok(RetireCircuits::All)
        } else {
            Ok(RetireCircuits::None)
        }
    }

    /// Return a [`Vanguard`] relay for use in the specified layer.
    ///
    /// The `relay_selector` must exclude the relays that would neighbor this vanguard
    /// in the path.
    ///
    /// Specifically, it should exclude
    ///   * the last relay in the path (the one immediately preceding the vanguard): the same relay
    ///     cannot be used in consecutive positions in the path (a relay won't let you extend the
    ///     circuit to itself).
    ///   * the penultimate relay of the path, if there is one: relays don't allow extending the
    ///     circuit to their previous hop
    ///
    /// If [`Full`](VanguardMode::Full) vanguards are in use, this function can be used
    /// for selecting both [`Layer2`](Layer::Layer2) and [`Layer3`](Layer::Layer3) vanguards.
    ///
    /// If [`Lite`](VanguardMode::Lite) vanguards are in use, this function can only be used
    /// for selecting [`Layer2`](Layer::Layer2) vanguards.
    /// It will return an error if a [`Layer3`](Layer::Layer3) is requested.
    ///
    /// Returns an error if vanguards are disabled.
    ///
    /// Returns a [`NoSuitableRelay`](VanguardMgrError::NoSuitableRelay) error
    /// if none of our vanguards satisfy the `layer` and `neighbor_exclusion` requirements.
    ///
    /// Returns a [`BootstrapRequired`](VanguardMgrError::BootstrapRequired) error
    /// if called before the vanguard manager has finished bootstrapping,
    /// or if all the vanguards have become unusable
    /// (by expiring or no longer being listed in the consensus)
    /// and we are unable to replenish them.
    ///
    ///  ### Example
    ///
    ///  If the partially built path is of the form `G - L2` and we are selecting the L3 vanguard,
    ///  the `RelayExclusion` should contain `G` and `L2` (to prevent building a path of the form
    ///  `G - L2 - G`, or `G - L2 - L2`).
    ///
    ///  If the path only contains the L1 guard (`G`), then the `RelayExclusion` should only
    ///  exclude `G`.
    pub fn select_vanguard<'a, Rng: RngCore>(
        &self,
        rng: &mut Rng,
        netdir: &'a NetDir,
        layer: Layer,
        relay_selector: &RelaySelector<'a>,
    ) -> Result<Vanguard<'a>, VanguardMgrError> {
        use VanguardMode::*;

        let inner = self.inner.read().expect("poisoned lock");

        // All our vanguard sets are empty. This means select_vanguards() was called before
        // maintain_vanguard_sets() managed to obtain a netdir and populate the vanguard sets,
        // or all the vanguards have become unusable and we have been unable to replenish them.
        if inner.vanguard_sets.l2().is_empty() && inner.vanguard_sets.l3().is_empty() {
            return Err(VanguardMgrError::BootstrapRequired {
                action: "select vanguard",
            });
        }

        let relay =
            match (layer, inner.mode) {
                (Layer::Layer2, Full) | (Layer::Layer2, Lite) => inner
                    .vanguard_sets
                    .l2()
                    .pick_relay(rng, netdir, relay_selector),
                (Layer::Layer3, Full) => {
                    inner
                        .vanguard_sets
                        .l3()
                        .pick_relay(rng, netdir, relay_selector)
                }
                _ => {
                    return Err(VanguardMgrError::LayerNotSupported {
                        layer,
                        mode: inner.mode,
                    });
                }
            };

        relay.ok_or(VanguardMgrError::NoSuitableRelay(layer))
    }

    /// The vanguard set management task.
    ///
    /// This is a background task that:
    /// * removes vanguards from the L2 and L3 vanguard sets when they expire
    /// * ensures the vanguard sets are repopulated with new vanguards
    ///   when the number of vanguards drops below a certain threshold
    /// * handles `NetDir` changes, updating the vanguard set sizes as needed
    async fn maintain_vanguard_sets(
        mgr: Weak<Self>,
        netdir_provider: Weak<dyn NetDirProvider>,
        mut config_rx: watch::Receiver<VanguardConfig>,
    ) {
        let mut netdir_events = match netdir_provider.upgrade() {
            Some(provider) => provider.events(),
            None => {
                return;
            }
        };

        loop {
            match Self::run_once(
                Weak::clone(&mgr),
                Weak::clone(&netdir_provider),
                &mut netdir_events,
                &mut config_rx,
            )
            .await
            {
                Ok(ShutdownStatus::Continue) => continue,
                Ok(ShutdownStatus::Terminate) => {
                    debug!("Vanguard manager is shutting down");
                    break;
                }
                Err(e) => {
                    error_report!(e, "Vanguard manager crashed");
                    break;
                }
            }
        }
    }

    /// Wait until a vanguard expires or until there is a new [`NetDir`].
    ///
    /// This populates the L2 and L3 vanguard sets,
    /// and rotates the vanguards when their lifetime expires.
    ///
    /// Note: the L3 set is only populated with vanguards if
    /// [`Full`](VanguardMode::Full) vanguards are enabled.
    async fn run_once(
        mgr: Weak<Self>,
        netdir_provider: Weak<dyn NetDirProvider>,
        netdir_events: &mut BoxStream<'static, DirEvent>,
        config_rx: &mut watch::Receiver<VanguardConfig>,
    ) -> Result<ShutdownStatus, VanguardMgrError> {
        let (mgr, netdir_provider) = match (mgr.upgrade(), netdir_provider.upgrade()) {
            (Some(mgr), Some(netdir_provider)) => (mgr, netdir_provider),
            _ => return Ok(ShutdownStatus::Terminate),
        };

        let now = mgr.runtime.wallclock();
        let next_to_expire = mgr.rotate_expired(&netdir_provider, now)?;
        // A future that sleeps until the next vanguard expires
        let sleep_fut = async {
            if let Some(dur) = next_to_expire {
                let () = mgr.runtime.sleep(dur).await;
            } else {
                future::pending::<()>().await;
            }
        };

        select_biased! {
            event = netdir_events.next().fuse() => {
                if let Some(DirEvent::NewConsensus) = event {
                    let netdir = netdir_provider.netdir(Timeliness::Timely)?;
                    mgr.inner.write().expect("poisoned lock")
                        .update_vanguard_sets(&mgr.runtime, &mgr.storage, &netdir)?;
                }

                Ok(ShutdownStatus::Continue)
            },
            _config = config_rx.recv().fuse() => {
                if let Some(netdir) = Self::timely_netdir(&netdir_provider)? {
                    // If we have a NetDir, replenish the vanguard sets that don't have enough vanguards.
                    //
                    // For example, if the config change enables full vanguards for the first time,
                    // this will cause the L3 vanguard set to be populated.
                    mgr.inner.write().expect("poisoned lock")
                        .update_vanguard_sets(&mgr.runtime, &mgr.storage, &netdir)?;
                }

                Ok(ShutdownStatus::Continue)
            },
            () = sleep_fut.fuse() => {
                // A vanguard expired, time to run the cleanup
                Ok(ShutdownStatus::Continue)
            },
        }
    }

    /// Return a timely `NetDir`, if one is available.
    ///
    /// Returns `None` if no directory information is available.
    fn timely_netdir(
        netdir_provider: &Arc<dyn NetDirProvider>,
    ) -> Result<Option<Arc<NetDir>>, VanguardMgrError> {
        use tor_netdir::Error as NetDirError;

        match netdir_provider.netdir(Timeliness::Timely) {
            Ok(netdir) => Ok(Some(netdir)),
            Err(NetDirError::NoInfo) | Err(NetDirError::NotEnoughInfo) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Rotate the vanguards that have expired,
    /// returning how long until the next vanguard will expire,
    /// or `None` if there are no vanguards in any of our sets.
    fn rotate_expired(
        &self,
        netdir_provider: &Arc<dyn NetDirProvider>,
        now: SystemTime,
    ) -> Result<Option<Duration>, VanguardMgrError> {
        let mut inner = self.inner.write().expect("poisoned lock");
        let inner = &mut *inner;

        let vanguard_sets = &mut inner.vanguard_sets;
        let expired_count = vanguard_sets.remove_expired(now);

        if expired_count > 0 {
            info!("Rotating vanguards");
        }

        if let Some(netdir) = Self::timely_netdir(netdir_provider)? {
            // If we have a NetDir, replenish the vanguard sets that don't have enough vanguards.
            inner.update_vanguard_sets(&self.runtime, &self.storage, &netdir)?;
        }

        let Some(expiry) = inner.vanguard_sets.next_expiry() else {
            // Both vanguard sets are empty
            return Ok(None);
        };

        expiry
            .duration_since(now)
            .map_err(|_| internal!("when > now, but now is later than when?!").into())
            .map(Some)
    }

    /// Get the current [`VanguardMode`].
    pub fn mode(&self) -> VanguardMode {
        self.inner.read().expect("poisoned lock").mode
    }
}

impl Inner {
    /// Update the vanguard sets, handling any potential vanguard parameter changes.
    ///
    /// This updates the [`VanguardSets`]s based on the [`VanguardParams`]
    /// derived from the new `NetDir`, replenishing the sets if necessary.
    ///
    /// NOTE: if the new `VanguardParams` specify different lifetime ranges
    /// than the previous `VanguardParams`, the new lifetime requirements only
    /// apply to newly selected vanguards. They are **not** retroactively applied
    /// to our existing vanguards.
    //
    // TODO(#1352): we might want to revisit this decision.
    // We could, for example, adjust the lifetime of our existing vanguards
    // to comply with the new lifetime requirements.
    fn update_vanguard_sets<R: Runtime>(
        &mut self,
        runtime: &R,
        storage: &DynStorageHandle<VanguardSets>,
        netdir: &Arc<NetDir>,
    ) -> Result<(), VanguardMgrError> {
        let params = VanguardParams::try_from(netdir.params())
            .map_err(into_internal!("invalid NetParameters"))?;

        // Update our params with the new values.
        self.update_params(params.clone());

        self.vanguard_sets.remove_unlisted(netdir);

        // If we loaded some vanguards from persistent storage but we still need more,
        // we select them here.
        //
        // If full vanguards are not enabled and we started with an empty (default)
        // vanguard set, we populate the sets here.
        //
        // If we have already populated the vanguard sets in a previous iteration,
        // this will ensure they have enough vanguards.
        self.vanguard_sets
            .replenish_vanguards(runtime, netdir, &params, self.mode)?;

        // Flush the vanguard sets to disk.
        self.flush_to_storage(storage)?;

        Ok(())
    }

    /// Update our vanguard params.
    fn update_params(&mut self, new_params: VanguardParams) {
        self.params = new_params;
    }

    /// Flush the vanguard sets to storage, if the mode is "vanguards-full".
    fn flush_to_storage(
        &self,
        storage: &DynStorageHandle<VanguardSets>,
    ) -> Result<(), VanguardMgrError> {
        match self.mode {
            VanguardMode::Lite | VanguardMode::Disabled => Ok(()),
            VanguardMode::Full => {
                debug!("The vanguards may have changed; flushing to vanguard state file");
                Ok(storage.store(&self.vanguard_sets)?)
            }
        }
    }
}

#[cfg(any(test, feature = "testing"))]
use {
    tor_config::ExplicitOrAuto, tor_netdir::testprovider::TestNetDirProvider,
    tor_persist::TestingStateMgr, tor_rtmock::MockRuntime,
};

/// Helpers for tests involving vanguards
#[cfg(any(test, feature = "testing"))]
impl VanguardMgr<MockRuntime> {
    /// Create a new VanguardMgr for testing.
    pub fn new_testing(
        rt: &MockRuntime,
        mode: VanguardMode,
    ) -> Result<Arc<VanguardMgr<MockRuntime>>, VanguardMgrError> {
        let config = VanguardConfig {
            mode: ExplicitOrAuto::Explicit(mode),
        };
        let statemgr = TestingStateMgr::new();
        let lock = statemgr.try_lock()?;
        assert!(lock.held());
        // TODO(#1382): has_onion_svc doesn't matter right now
        let has_onion_svc = false;
        Ok(Arc::new(VanguardMgr::new(
            &config,
            rt.clone(),
            statemgr,
            has_onion_svc,
        )?))
    }

    /// Wait until the vanguardmgr has populated its vanguard sets.
    ///
    /// Returns a [`TestNetDirProvider`] that can be used to notify
    /// the `VanguardMgr` of netdir changes.
    pub async fn init_vanguard_sets(
        self: &Arc<VanguardMgr<MockRuntime>>,
        netdir: &NetDir,
    ) -> Result<Arc<TestNetDirProvider>, VanguardMgrError> {
        let netdir_provider = Arc::new(TestNetDirProvider::new());
        self.launch_background_tasks(&(netdir_provider.clone() as Arc<dyn NetDirProvider>))?;
        self.runtime.progress_until_stalled().await;

        // Call set_netdir_and_notify to trigger an event
        netdir_provider
            .set_netdir_and_notify(Arc::new(netdir.clone()))
            .await;

        // Wait until the vanguard mgr has finished handling the netdir event.
        self.runtime.progress_until_stalled().await;

        Ok(netdir_provider)
    }
}

/// The vanguard layer.
#[derive(Debug, Clone, Copy, PartialEq)] //
#[derive(derive_more::Display)] //
#[non_exhaustive]
pub enum Layer {
    /// L2 vanguard.
    #[display("layer 2")]
    Layer2,
    /// L3 vanguard.
    #[display("layer 3")]
    Layer3,
}

#[cfg(test)]
mod test {
    // @@ begin test lint list maintained by maint/add_warning @@
    #![allow(clippy::bool_assert_comparison)]
    #![allow(clippy::clone_on_copy)]
    #![allow(clippy::dbg_macro)]
    #![allow(clippy::mixed_attributes_style)]
    #![allow(clippy::print_stderr)]
    #![allow(clippy::print_stdout)]
    #![allow(clippy::single_char_pattern)]
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::unchecked_duration_subtraction)]
    #![allow(clippy::useless_vec)]
    #![allow(clippy::needless_pass_by_value)]
    //! <!-- @@ end test lint list maintained by maint/add_warning @@ -->

    use std::{fmt, time};

    use set::TimeBoundVanguard;
    use tor_config::ExplicitOrAuto;
    use tor_relay_selection::RelayExclusion;

    use super::*;

    use tor_basic_utils::test_rng::testing_rng;
    use tor_linkspec::{HasRelayIds, RelayIds};
    use tor_netdir::{
        testnet::{self, construct_custom_netdir_with_params},
        testprovider::TestNetDirProvider,
    };
    use tor_persist::FsStateMgr;
    use tor_rtmock::MockRuntime;
    use Layer::*;

    use itertools::Itertools;

    /// Enable lite vanguards for onion services.
    const ENABLE_LITE_VANGUARDS: [(&str, i32); 1] = [("vanguards-hs-service", 1)];

    /// Enable full vanguards for hidden services.
    const ENABLE_FULL_VANGUARDS: [(&str, i32); 1] = [("vanguards-hs-service", 2)];

    /// A valid vanguard state file.
    const VANGUARDS_JSON: &str = include_str!("../testdata/vanguards.json");

    /// A invalid vanguard state file.
    const INVALID_VANGUARDS_JSON: &str = include_str!("../testdata/vanguards_invalid.json");

    /// Create the `StateMgr`, populating the vanguards.json state file with the specified JSON string.
    fn state_dir_with_vanguards(vanguards_json: &str) -> (FsStateMgr, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(dir.path().join("state")).unwrap();
        std::fs::write(dir.path().join("state/vanguards.json"), vanguards_json).unwrap();

        let statemgr = FsStateMgr::from_path_and_mistrust(
            dir.path(),
            &fs_mistrust::Mistrust::new_dangerously_trust_everyone(),
        )
        .unwrap();

        (statemgr, dir)
    }

    impl fmt::Debug for Vanguard<'_> {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            f.debug_struct("Vanguard").finish()
        }
    }

    impl Inner {
        /// Return the L2 vanguard set.
        pub(super) fn l2_vanguards(&self) -> &Vec<TimeBoundVanguard> {
            self.vanguard_sets.l2_vanguards()
        }

        /// Return the L3 vanguard set.
        pub(super) fn l3_vanguards(&self) -> &Vec<TimeBoundVanguard> {
            self.vanguard_sets.l3_vanguards()
        }
    }

    /// Return a maximally permissive RelaySelector for a vanguard.
    fn permissive_selector() -> RelaySelector<'static> {
        RelaySelector::new(
            tor_relay_selection::RelayUsage::vanguard(),
            RelayExclusion::no_relays_excluded(),
        )
    }

    /// Look up the vanguard in the specified VanguardSet.
    fn find_in_set<R: Runtime>(
        relay_ids: &RelayIds,
        mgr: &VanguardMgr<R>,
        layer: Layer,
    ) -> Option<TimeBoundVanguard> {
        let inner = mgr.inner.read().unwrap();

        let vanguards = match layer {
            Layer2 => inner.l2_vanguards(),
            Layer3 => inner.l3_vanguards(),
        };

        // Look up the TimeBoundVanguard that corresponds to this Vanguard,
        // and figure out its expiry.
        vanguards.iter().find(|v| v.id == *relay_ids).cloned()
    }

    /// Get the total number of vanguard entries (L2 + L3).
    fn vanguard_count<R: Runtime>(mgr: &VanguardMgr<R>) -> usize {
        let inner = mgr.inner.read().unwrap();
        inner.l2_vanguards().len() + inner.l3_vanguards().len()
    }

    /// Return a `Duration` representing how long until this vanguard expires.
    fn duration_until_expiry<R: Runtime>(
        relay_ids: &RelayIds,
        mgr: &VanguardMgr<R>,
        runtime: &R,
        layer: Layer,
    ) -> Duration {
        // Look up the TimeBoundVanguard that corresponds to this Vanguard,
        // and figure out its expiry.
        let vanguard = find_in_set(relay_ids, mgr, layer).unwrap();

        vanguard
            .when
            .duration_since(runtime.wallclock())
            .unwrap_or_default()
    }

    /// Assert the lifetime of the specified `vanguard` is within the bounds of its `layer`.
    fn assert_expiry_in_bounds<R: Runtime>(
        vanguard: &Vanguard<'_>,
        mgr: &VanguardMgr<R>,
        runtime: &R,
        params: &VanguardParams,
        layer: Layer,
    ) {
        let (min, max) = match layer {
            Layer2 => (params.l2_lifetime_min(), params.l2_lifetime_max()),
            Layer3 => (params.l3_lifetime_min(), params.l3_lifetime_max()),
        };

        let vanguard = RelayIds::from_relay_ids(vanguard.relay());
        // This is not exactly the lifetime of the vanguard,
        // but rather the time left until it expires (but it's close enough for our purposes).
        let lifetime = duration_until_expiry(&vanguard, mgr, runtime, layer);

        assert!(
            lifetime >= min && lifetime <= max,
            "lifetime {lifetime:?} not between {min:?} and {max:?}",
        );
    }

    /// Assert that the vanguard manager's pools are empty.
    fn assert_sets_empty<R: Runtime>(vanguardmgr: &VanguardMgr<R>) {
        let inner = vanguardmgr.inner.read().unwrap();
        // The sets are initially empty, and the targets are set to 0
        assert_eq!(inner.vanguard_sets.l2_vanguards_deficit(), 0);
        assert_eq!(inner.vanguard_sets.l3_vanguards_deficit(), 0);
        assert_eq!(vanguard_count(vanguardmgr), 0);
    }

    /// Assert that the vanguard manager's pools have been filled.
    fn assert_sets_filled<R: Runtime>(vanguardmgr: &VanguardMgr<R>, params: &VanguardParams) {
        let inner = vanguardmgr.inner.read().unwrap();
        let l2_pool_size = params.l2_pool_size();
        // The sets are initially empty
        assert_eq!(inner.vanguard_sets.l2_vanguards_deficit(), 0);

        if inner.mode == VanguardMode::Full {
            assert_eq!(inner.vanguard_sets.l3_vanguards_deficit(), 0);
            let l3_pool_size = params.l3_pool_size();
            assert_eq!(vanguard_count(vanguardmgr), l2_pool_size + l3_pool_size);
        }
    }

    /// Assert the target size of the specified vanguard set matches the target from `params`.
    fn assert_set_vanguards_targets_match_params<R: Runtime>(
        mgr: &VanguardMgr<R>,
        params: &VanguardParams,
    ) {
        let inner = mgr.inner.read().unwrap();
        assert_eq!(
            inner.vanguard_sets.l2_vanguards_target(),
            params.l2_pool_size()
        );
        if inner.mode == VanguardMode::Full {
            assert_eq!(
                inner.vanguard_sets.l3_vanguards_target(),
                params.l3_pool_size()
            );
        }
    }

    #[test]
    fn full_vanguards_disabled() {
        MockRuntime::test_with_various(|rt| async move {
            let vanguardmgr = VanguardMgr::new_testing(&rt, VanguardMode::Lite).unwrap();
            let netdir = testnet::construct_netdir().unwrap_if_sufficient().unwrap();
            let mut rng = testing_rng();
            // Wait until the vanguard manager has bootstrapped
            // (otherwise we'll get a BootstrapRequired error)
            let _netdir_provider = vanguardmgr.init_vanguard_sets(&netdir).await.unwrap();

            // Cannot select an L3 vanguard when running in "Lite" mode.
            let err = vanguardmgr
                .select_vanguard(&mut rng, &netdir, Layer3, &permissive_selector())
                .unwrap_err();
            assert!(
                matches!(
                    err,
                    VanguardMgrError::LayerNotSupported {
                        layer: Layer::Layer3,
                        mode: VanguardMode::Lite
                    }
                ),
                "{err}"
            );
        });
    }

    #[test]
    fn background_task_not_spawned() {
        MockRuntime::test_with_various(|rt| async move {
            let vanguardmgr = VanguardMgr::new_testing(&rt, VanguardMode::Lite).unwrap();
            let netdir = testnet::construct_netdir().unwrap_if_sufficient().unwrap();
            let mut rng = testing_rng();

            // The sets are initially empty
            assert_sets_empty(&vanguardmgr);

            // VanguardMgr::launch_background tasks was not called, so select_vanguard will return
            // an error (because the vanguard sets are empty)
            let err = vanguardmgr
                .select_vanguard(&mut rng, &netdir, Layer2, &permissive_selector())
                .unwrap_err();

            assert!(
                matches!(
                    err,
                    VanguardMgrError::BootstrapRequired {
                        action: "select vanguard"
                    }
                ),
                "{err:?}"
            );
        });
    }

    #[test]
    fn select_vanguards() {
        MockRuntime::test_with_various(|rt| async move {
            let vanguardmgr = VanguardMgr::new_testing(&rt, VanguardMode::Full).unwrap();

            let netdir = testnet::construct_netdir().unwrap_if_sufficient().unwrap();
            let params = VanguardParams::try_from(netdir.params()).unwrap();
            let mut rng = testing_rng();

            // The sets are initially empty
            assert_sets_empty(&vanguardmgr);

            // Wait until the vanguard manager has bootstrapped
            let _netdir_provider = vanguardmgr.init_vanguard_sets(&netdir).await.unwrap();

            assert_sets_filled(&vanguardmgr, &params);

            let vanguard1 = vanguardmgr
                .select_vanguard(&mut rng, &netdir, Layer2, &permissive_selector())
                .unwrap();
            assert_expiry_in_bounds(&vanguard1, &vanguardmgr, &rt, &params, Layer2);

            let exclusion = RelayExclusion::exclude_identities(
                vanguard1
                    .relay()
                    .identities()
                    .map(|id| id.to_owned())
                    .collect(),
            );
            let selector =
                RelaySelector::new(tor_relay_selection::RelayUsage::vanguard(), exclusion);

            let vanguard2 = vanguardmgr
                .select_vanguard(&mut rng, &netdir, Layer3, &selector)
                .unwrap();

            assert_expiry_in_bounds(&vanguard2, &vanguardmgr, &rt, &params, Layer3);
            // Ensure we didn't select the same vanguard twice
            assert_ne!(
                vanguard1.relay().identities().collect_vec(),
                vanguard2.relay().identities().collect_vec()
            );
        });
    }

    /// Override the vanguard params from the netdir, returning the new VanguardParams.
    ///
    /// This also waits until the vanguard manager has had a chance to process the changes.
    async fn install_new_params(
        rt: &MockRuntime,
        netdir_provider: &TestNetDirProvider,
        params: impl IntoIterator<Item = (&str, i32)>,
    ) -> VanguardParams {
        let new_netdir = testnet::construct_custom_netdir_with_params(|_, _, _| {}, params, None)
            .unwrap()
            .unwrap_if_sufficient()
            .unwrap();
        let new_params = VanguardParams::try_from(new_netdir.params()).unwrap();

        netdir_provider.set_netdir_and_notify(new_netdir).await;

        // Wait until the vanguard mgr has finished handling the new netdir.
        rt.progress_until_stalled().await;

        new_params
    }

    /// Switch the vanguard "mode" of the VanguardMgr to `mode`,
    /// by setting the vanguards-hs-service parameter.
    //
    // TODO(#1382): use this instead of switch_hs_mode_config.
    #[allow(unused)]
    async fn switch_hs_mode(
        rt: &MockRuntime,
        vanguardmgr: &VanguardMgr<MockRuntime>,
        netdir_provider: &TestNetDirProvider,
        mode: VanguardMode,
    ) {
        use VanguardMode::*;

        let _params = match mode {
            Lite => install_new_params(rt, netdir_provider, ENABLE_LITE_VANGUARDS).await,
            Full => install_new_params(rt, netdir_provider, ENABLE_FULL_VANGUARDS).await,
            Disabled => panic!("cannot disable vanguards in the vanguard tests!"),
        };

        assert_eq!(vanguardmgr.mode(), mode);
    }

    /// Switch the vanguard "mode" of the VanguardMgr to `mode`,
    /// by calling `VanguardMgr::reconfigure`.
    fn switch_hs_mode_config(vanguardmgr: &VanguardMgr<MockRuntime>, mode: VanguardMode) {
        let _ = vanguardmgr
            .reconfigure(&VanguardConfig {
                mode: ExplicitOrAuto::Explicit(mode),
            })
            .unwrap();

        assert_eq!(vanguardmgr.mode(), mode);
    }

    /// Use a new NetDir that excludes one of our L2 vanguards
    async fn install_netdir_excluding_vanguard<'a>(
        runtime: &MockRuntime,
        vanguard: &Vanguard<'_>,
        params: impl IntoIterator<Item = (&'a str, i32)>,
        netdir_provider: &TestNetDirProvider,
    ) -> NetDir {
        let new_netdir = construct_custom_netdir_with_params(
            |_idx, bld, _| {
                let md_so_far = bld.md.testing_md().unwrap();
                if md_so_far.ed25519_id() == vanguard.relay().id() {
                    bld.omit_rs = true;
                }
            },
            params,
            None,
        )
        .unwrap()
        .unwrap_if_sufficient()
        .unwrap();

        netdir_provider
            .set_netdir_and_notify(new_netdir.clone())
            .await;
        // Wait until the vanguard mgr has finished handling the new netdir.
        runtime.progress_until_stalled().await;

        new_netdir
    }

    #[test]
    fn override_vanguard_set_size() {
        MockRuntime::test_with_various(|rt| async move {
            let vanguardmgr = VanguardMgr::new_testing(&rt, VanguardMode::Lite).unwrap();
            let netdir = testnet::construct_netdir().unwrap_if_sufficient().unwrap();
            // Wait until the vanguard manager has bootstrapped
            let netdir_provider = vanguardmgr.init_vanguard_sets(&netdir).await.unwrap();

            let params = VanguardParams::try_from(netdir.params()).unwrap();
            let old_size = params.l2_pool_size();
            assert_set_vanguards_targets_match_params(&vanguardmgr, &params);

            const PARAMS: [[(&str, i32); 2]; 2] = [
                [("guard-hs-l2-number", 1), ("guard-hs-l3-number", 10)],
                [("guard-hs-l2-number", 10), ("guard-hs-l3-number", 10)],
            ];

            for params in PARAMS {
                let new_params = install_new_params(&rt, &netdir_provider, params).await;

                // Ensure the target size was updated.
                assert_set_vanguards_targets_match_params(&vanguardmgr, &new_params);
                {
                    let inner = vanguardmgr.inner.read().unwrap();
                    let l2_vanguards = inner.l2_vanguards();
                    let l3_vanguards = inner.l3_vanguards();
                    let new_l2_size = params[0].1 as usize;
                    if new_l2_size < old_size {
                        // The actual size of the set hasn't changed: it's OK to have more vanguards than
                        // needed in the set (they extraneous ones will eventually expire).
                        assert_eq!(l2_vanguards.len(), old_size);
                    } else {
                        // The new size is greater, so we have more L2 vanguards now.
                        assert_eq!(l2_vanguards.len(), new_l2_size);
                    }
                    // There are no L3 vanguards because full vanguards are not in use.
                    assert_eq!(l3_vanguards.len(), 0);
                }
            }
        });
    }

    #[test]
    fn expire_vanguards() {
        MockRuntime::test_with_various(|rt| async move {
            let vanguardmgr = VanguardMgr::new_testing(&rt, VanguardMode::Lite).unwrap();
            let netdir = testnet::construct_netdir().unwrap_if_sufficient().unwrap();
            let params = VanguardParams::try_from(netdir.params()).unwrap();
            let initial_l2_number = params.l2_pool_size();

            // Wait until the vanguard manager has bootstrapped
            let netdir_provider = vanguardmgr.init_vanguard_sets(&netdir).await.unwrap();
            assert_eq!(vanguard_count(&vanguardmgr), params.l2_pool_size());

            // Find the RelayIds of the vanguard that is due to expire next
            let vanguard_id = {
                let inner = vanguardmgr.inner.read().unwrap();
                let next_expiry = inner.vanguard_sets.next_expiry().unwrap();
                inner
                    .l2_vanguards()
                    .iter()
                    .find(|v| v.when == next_expiry)
                    .cloned()
                    .unwrap()
                    .id
            };

            const FEWER_VANGUARDS_PARAM: [(&str, i32); 1] = [("guard-hs-l2-number", 1)];
            // Set the number of L2 vanguards to a lower value to ensure the vanguard that is about
            // to expire is not replaced. This allows us to test that it has indeed expired
            // (we can't simply check that the relay is no longer is the set,
            // because it's possible for the set to get replenished with the same relay).
            let new_params = install_new_params(&rt, &netdir_provider, FEWER_VANGUARDS_PARAM).await;

            // The vanguard has not expired yet.
            let timebound_vanguard = find_in_set(&vanguard_id, &vanguardmgr, Layer2);
            assert!(timebound_vanguard.is_some());
            assert_eq!(vanguard_count(&vanguardmgr), initial_l2_number);

            let lifetime = duration_until_expiry(&vanguard_id, &vanguardmgr, &rt, Layer2);
            // Wait until this vanguard expires
            rt.advance_by(lifetime).await.unwrap();
            rt.progress_until_stalled().await;

            let timebound_vanguard = find_in_set(&vanguard_id, &vanguardmgr, Layer2);

            // The vanguard expired, but was not replaced.
            assert!(timebound_vanguard.is_none());
            assert_eq!(vanguard_count(&vanguardmgr), initial_l2_number - 1);

            // Wait until more vanguards expire. This will reduce the set size to 1
            // (the new target size we set by overriding the params).
            for _ in 0..initial_l2_number - 1 {
                let vanguard_id = {
                    let inner = vanguardmgr.inner.read().unwrap();
                    let next_expiry = inner.vanguard_sets.next_expiry().unwrap();
                    inner
                        .l2_vanguards()
                        .iter()
                        .find(|v| v.when == next_expiry)
                        .cloned()
                        .unwrap()
                        .id
                };
                let lifetime = duration_until_expiry(&vanguard_id, &vanguardmgr, &rt, Layer2);
                rt.advance_by(lifetime).await.unwrap();

                rt.progress_until_stalled().await;
            }

            assert_eq!(vanguard_count(&vanguardmgr), new_params.l2_pool_size());

            // Update the L2 set size again, to force the vanguard manager to replenish the L2 set.
            const MORE_VANGUARDS_PARAM: [(&str, i32); 1] = [("guard-hs-l2-number", 5)];
            // Set the number of L2 vanguards to a lower value to ensure the vanguard that is about
            // to expire is not replaced. This allows us to test that it has indeed expired
            // (we can't simply check that the relay is no longer is the set,
            // because it's possible for the set to get replenished with the same relay).
            let new_params = install_new_params(&rt, &netdir_provider, MORE_VANGUARDS_PARAM).await;

            // Check that we replaced the expired vanguard with a new one:
            assert_eq!(vanguard_count(&vanguardmgr), new_params.l2_pool_size());

            {
                let inner = vanguardmgr.inner.read().unwrap();
                let l2_count = inner.l2_vanguards().len();
                assert_eq!(l2_count, new_params.l2_pool_size());
            }
        });
    }

    #[test]
    fn full_vanguards_persistence() {
        MockRuntime::test_with_various(|rt| async move {
            let vanguardmgr = VanguardMgr::new_testing(&rt, VanguardMode::Lite).unwrap();

            let netdir =
                construct_custom_netdir_with_params(|_, _, _| {}, ENABLE_LITE_VANGUARDS, None)
                    .unwrap()
                    .unwrap_if_sufficient()
                    .unwrap();
            let netdir_provider = vanguardmgr.init_vanguard_sets(&netdir).await.unwrap();

            // Full vanguards are not enabled, so we don't expect anything to be written
            // to persistent storage.
            assert_eq!(vanguardmgr.mode(), VanguardMode::Lite);
            assert!(vanguardmgr.storage.load().unwrap().is_none());

            let mut rng = testing_rng();
            assert!(vanguardmgr
                .select_vanguard(&mut rng, &netdir, Layer3, &permissive_selector())
                .is_err());

            // Enable full vanguards again.
            //
            // We expect VanguardMgr to populate the L3 set, and write the VanguardSets to storage.
            switch_hs_mode_config(&vanguardmgr, VanguardMode::Full);
            rt.progress_until_stalled().await;

            let vanguard_sets_orig = vanguardmgr.storage.load().unwrap();
            assert!(vanguardmgr
                .select_vanguard(&mut rng, &netdir, Layer3, &permissive_selector())
                .is_ok());

            // Switch to lite vanguards.
            switch_hs_mode_config(&vanguardmgr, VanguardMode::Lite);

            // The vanguard sets should not change when switching between lite and full vanguards.
            assert_eq!(vanguard_sets_orig, vanguardmgr.storage.load().unwrap());
            switch_hs_mode_config(&vanguardmgr, VanguardMode::Full);
            assert_eq!(vanguard_sets_orig, vanguardmgr.storage.load().unwrap());

            // TODO HS-VANGUARDS: we may want to disable the ability to switch back to lite
            // vanguards.

            // Switch to lite vanguards and remove a relay from the consensus.
            // The relay should *not* be persisted to storage until we switch back to full
            // vanguards.
            switch_hs_mode_config(&vanguardmgr, VanguardMode::Lite);

            let mut rng = testing_rng();
            let excluded_vanguard = vanguardmgr
                .select_vanguard(&mut rng, &netdir, Layer2, &permissive_selector())
                .unwrap();

            let _ = install_netdir_excluding_vanguard(
                &rt,
                &excluded_vanguard,
                ENABLE_LITE_VANGUARDS,
                &netdir_provider,
            )
            .await;

            // The vanguard sets from storage haven't changed, because we are in "lite" mode.
            assert_eq!(vanguard_sets_orig, vanguardmgr.storage.load().unwrap());
            let _ = install_netdir_excluding_vanguard(
                &rt,
                &excluded_vanguard,
                ENABLE_FULL_VANGUARDS,
                &netdir_provider,
            )
            .await;
        });
    }

    #[test]
    fn load_from_state_file() {
        MockRuntime::test_with_various(|rt| async move {
            // Set the wallclock to a time when some of the stored vanguards are still valid.
            let now = time::UNIX_EPOCH + Duration::from_secs(1610000000);
            rt.jump_wallclock(now);

            let config = VanguardConfig {
                mode: ExplicitOrAuto::Explicit(VanguardMode::Full),
            };

            // The state file contains no vanguards
            let (statemgr, _dir) =
                state_dir_with_vanguards(r#"{ "l2_vanguards": [], "l3_vanguards": [] }"#);
            let vanguardmgr = VanguardMgr::new(&config, rt.clone(), statemgr, false).unwrap();
            {
                let inner = vanguardmgr.inner.read().unwrap();

                // The vanguard sets should be empty too
                assert!(inner.vanguard_sets.l2().is_empty());
                assert!(inner.vanguard_sets.l3().is_empty());
            }

            let (statemgr, _dir) = state_dir_with_vanguards(VANGUARDS_JSON);
            let vanguardmgr =
                Arc::new(VanguardMgr::new(&config, rt.clone(), statemgr, false).unwrap());
            let (initial_l2, initial_l3) = {
                let inner = vanguardmgr.inner.read().unwrap();
                let l2_vanguards = inner.vanguard_sets.l2_vanguards().clone();
                let l3_vanguards = inner.vanguard_sets.l3_vanguards().clone();

                // The sets actually contain 4 and 5 vanguards, respectively,
                // but the expired ones are discarded.
                assert_eq!(l2_vanguards.len(), 3);
                assert_eq!(l3_vanguards.len(), 2);
                // We don't know how many vanguards we're going to need
                // until we fetch the consensus.
                assert_eq!(inner.vanguard_sets.l2_vanguards_target(), 0);
                assert_eq!(inner.vanguard_sets.l3_vanguards_target(), 0);
                assert_eq!(inner.vanguard_sets.l2_vanguards_deficit(), 0);
                assert_eq!(inner.vanguard_sets.l3_vanguards_deficit(), 0);

                (l2_vanguards, l3_vanguards)
            };

            let netdir = testnet::construct_netdir().unwrap_if_sufficient().unwrap();
            let _netdir_provider = vanguardmgr.init_vanguard_sets(&netdir).await.unwrap();
            {
                let inner = vanguardmgr.inner.read().unwrap();
                let l2_vanguards = inner.vanguard_sets.l2_vanguards();
                let l3_vanguards = inner.vanguard_sets.l3_vanguards();

                // The sets were replenished with more vanguards
                assert_eq!(l2_vanguards.len(), 4);
                assert_eq!(l3_vanguards.len(), 8);
                // We now know we need 4 L2 vanguards and 8 L3 ones.
                assert_eq!(inner.vanguard_sets.l2_vanguards_target(), 4);
                assert_eq!(inner.vanguard_sets.l3_vanguards_target(), 8);
                assert_eq!(inner.vanguard_sets.l2_vanguards_deficit(), 0);
                assert_eq!(inner.vanguard_sets.l3_vanguards_deficit(), 0);

                // All of the vanguards read from the state file should still be in the sets.
                assert!(initial_l2.iter().all(|v| l2_vanguards.contains(v)));
                assert!(initial_l3.iter().all(|v| l3_vanguards.contains(v)));
            }
        });
    }

    #[test]
    fn invalid_state_file() {
        MockRuntime::test_with_various(|rt| async move {
            let config = VanguardConfig {
                mode: ExplicitOrAuto::Explicit(VanguardMode::Full),
            };
            let (statemgr, _dir) = state_dir_with_vanguards(INVALID_VANGUARDS_JSON);
            let res = VanguardMgr::new(&config, rt.clone(), statemgr, false);
            assert!(matches!(res, Err(VanguardMgrError::State(_))));
        });
    }
}
