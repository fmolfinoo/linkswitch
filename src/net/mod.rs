//! Everything that talks to the Windows networking stack.
//!
//! Read paths ([`adapters::enumerate`], [`metric::snapshot`], [`routes::default_routes`]) are
//! unprivileged and are called by the widget on every refresh. Write paths ([`metric::set`]) run
//! only in the elevated worker.

pub mod adapters;
pub mod binding;
pub mod err;
pub mod luid;
pub mod metric;
pub mod notify;
pub mod routes;
pub mod wcm;
pub mod wifi;

pub use luid::LuidKey;

/// A complete, consistent read of the machine's networking state.
///
/// Taken as one unit so the UI never renders a torn view -- an adapter list from before a cable
/// was pulled against a route table from after it.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub nics: Vec<adapters::Nic>,
    pub metrics: Vec<metric::IfaceMetric>,
    pub routes: Vec<routes::DefaultRoute>,
}

impl Snapshot {
    pub fn read() -> Self {
        let nics = adapters::enumerate();
        let metrics = metric::snapshot();
        let routes = routes::all_default_routes(&metrics);
        Self {
            nics,
            metrics,
            routes,
        }
    }

    pub fn nic(&self, luid: LuidKey) -> Option<&adapters::Nic> {
        self.nics.iter().find(|n| n.luid == luid)
    }

    /// Steerable candidates, best-first, split by kind.
    ///
    /// Ranking must go through [`adapters::rank`]; a filter-only copy of this that forgot to
    /// sort put two Wi-Fi Direct pseudo-adapters ahead of the real radio.
    pub fn candidates(&self) -> (Vec<&adapters::Nic>, Vec<&adapters::Nic>) {
        let of_kind = |k: adapters::NicKind| {
            let mut v: Vec<&adapters::Nic> = self
                .nics
                .iter()
                .filter(|n| n.kind == k && n.tier != adapters::Tier::Rejected)
                .collect();
            v.sort_by_key(|n| adapters::rank(n));
            v
        };
        (
            of_kind(adapters::NicKind::Ethernet),
            of_kind(adapters::NicKind::Wifi),
        )
    }

    pub fn verdict(&self, eth: Option<LuidKey>, wifi: Option<LuidKey>) -> routes::Verdict {
        routes::winner(&self.routes, eth, wifi)
    }
}
