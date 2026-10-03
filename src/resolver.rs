#![forbid(unsafe_code)]
//! Dependency-order resolver for the session service plan (spec 02 §3:
//! "start shell services in dependency order … so startup is parallel").
//!
//! Kahn's level-scheduling: each emitted level contains every service whose
//! dependencies are all satisfied by strictly earlier levels, so members of
//! one level may start concurrently. Ordering is deterministic (levels are
//! name-sorted) so boots are reproducible and tests are stable.
//!
//! Stop order is the mirror of start order: levels are drained in reverse
//! (dependents stop before their dependencies), matching systemd's
//! After=/Before= semantics for the user units we ship.

use crate::config::ServiceSpec;
use std::collections::{BTreeMap, BTreeSet, HashSet};

/// A resolved startup plan: level 0 starts first; everything in one level
/// can start in parallel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub levels: Vec<Vec<String>>,
}

impl Plan {
    /// Flat, dependency-safe single order (levels flattened in order).
    pub fn flat(&self) -> Vec<String> {
        self.levels.iter().flatten().cloned().collect()
    }

    /// Reverse of `flat`: safe shutdown order (dependents first).
    pub fn stop_order(&self) -> Vec<String> {
        let mut v = self.flat();
        v.reverse();
        v
    }

    /// The level index a service starts in.
    pub fn level_of(&self, name: &str) -> Option<usize> {
        self.levels
            .iter()
            .position(|lvl| lvl.iter().any(|n| n == name))
    }
}

/// Resolve a service list into a start plan.
///
/// Errors (fail closed): duplicate names, unknown dependencies, cycles.
pub fn resolve(services: &[ServiceSpec]) -> Result<Plan, String> {
    let mut seen = HashSet::new();
    for s in services {
        if !seen.insert(s.name.as_str()) {
            return Err(format!("duplicate service name {}", s.name));
        }
    }
    for s in services {
        for dep in &s.after {
            if !seen.contains(dep.as_str()) {
                return Err(format!("service {} depends on unknown {}", s.name, dep));
            }
        }
    }

    // name -> set of deps not yet satisfied
    let mut pending: BTreeMap<String, BTreeSet<String>> = services
        .iter()
        .map(|s| {
            let deps: BTreeSet<String> = s.after.iter().cloned().collect();
            (s.name.clone(), deps)
        })
        .collect();
    // name -> dependents (for decrementing)
    let dependents: BTreeMap<String, Vec<String>> = {
        let mut d: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for s in services {
            for dep in &s.after {
                d.entry(dep.clone()).or_default().push(s.name.clone());
            }
        }
        d
    };

    let mut levels: Vec<Vec<String>> = Vec::new();
    while !pending.is_empty() {
        // Everything with zero pending deps, name-sorted (BTreeMap keeps it).
        let ready: Vec<String> = pending
            .iter()
            .filter(|(_, deps)| deps.is_empty())
            .map(|(name, _)| name.clone())
            .collect();
        if ready.is_empty() {
            let stuck: Vec<String> = pending.keys().cloned().collect();
            return Err(format!("dependency cycle among: {}", stuck.join(", ")));
        }
        for name in &ready {
            pending.remove(name);
        }
        for name in &ready {
            if let Some(children) = dependents.get(name) {
                for child in children {
                    if let Some(deps) = pending.get_mut(child) {
                        deps.remove(name);
                    }
                }
            }
        }
        levels.push(ready);
    }

    Ok(Plan { levels })
}

/// Resolve only the subset needed in safe mode, preserving the dependency
/// edges between kept services (dropped dependencies are pruned; a kept
/// service whose dependency was dropped still starts — safe mode prefers
/// a usable minimal shell over strict ordering failures).
pub fn resolve_subset(services: &[ServiceSpec], keep: &HashSet<String>) -> Result<Plan, String> {
    let subset: Vec<ServiceSpec> = services
        .iter()
        .filter(|s| keep.contains(&s.name))
        .map(|s| {
            let mut s = s.clone();
            s.after.retain(|d| keep.contains(d));
            s
        })
        .collect();
    resolve(&subset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RestartPolicy;

    fn svc(name: &str, after: &[&str]) -> ServiceSpec {
        ServiceSpec {
            name: name.into(),
            unit: None,
            exec: Some(vec!["/bin/true".into()]),
            after: after.iter().map(|s| s.to_string()).collect(),
            restart: RestartPolicy::Always,
            ready_gate: false,
        }
    }

    #[test]
    fn empty_plan() {
        let p = resolve(&[]).unwrap();
        assert!(p.levels.is_empty());
        assert!(p.flat().is_empty());
    }

    #[test]
    fn chain_orders_levels() {
        let p = resolve(&[svc("c", &["b"]), svc("a", &[]), svc("b", &["a"])]).unwrap();
        assert_eq!(p.levels, vec![vec!["a"], vec!["b"], vec!["c"]]);
        assert_eq!(p.stop_order(), vec!["c", "b", "a"]);
    }

    #[test]
    fn independent_services_share_a_level() {
        let p = resolve(&[svc("b", &[]), svc("a", &[]), svc("c", &[])]).unwrap();
        assert_eq!(p.levels, vec![vec!["a", "b", "c"]]);
    }

    #[test]
    fn diamond() {
        // d depends on b and c; both depend on a.
        let p = resolve(&[
            svc("d", &["b", "c"]),
            svc("b", &["a"]),
            svc("c", &["a"]),
            svc("a", &[]),
        ])
        .unwrap();
        assert_eq!(p.levels, vec![vec!["a"], vec!["b", "c"], vec!["d"]]);
        assert_eq!(p.level_of("d"), Some(2));
    }

    #[test]
    fn cycle_detected() {
        let e = resolve(&[svc("a", &["b"]), svc("b", &["a"])]).unwrap_err();
        assert!(e.contains("cycle"), "got: {e}");
    }

    #[test]
    fn self_cycle_detected() {
        let e = resolve(&[svc("a", &["a"])]).unwrap_err();
        assert!(e.contains("cycle"), "got: {e}");
    }

    #[test]
    fn unknown_dep_detected() {
        let e = resolve(&[svc("a", &["ghost"])]).unwrap_err();
        assert!(e.contains("unknown ghost"), "got: {e}");
    }

    #[test]
    fn duplicate_detected() {
        let e = resolve(&[svc("a", &[]), svc("a", &[])]).unwrap_err();
        assert!(e.contains("duplicate"), "got: {e}");
    }

    #[test]
    fn subset_prunes_dangling_deps() {
        let keep: HashSet<String> = ["settings", "terminal"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        // terminal depends on panel which is NOT kept.
        let p = resolve_subset(
            &[
                svc("panel", &[]),
                svc("terminal", &["panel"]),
                svc("settings", &[]),
            ],
            &keep,
        )
        .unwrap();
        assert_eq!(p.flat(), vec!["settings", "terminal"]);
    }

    #[test]
    fn determinism() {
        let svcset: Vec<ServiceSpec> = (0..20)
            .map(|i| {
                let deps: Vec<String> = (0..i)
                    .filter(|j| (i * 7 + j) % 3 == 0)
                    .map(|j| format!("s{j}"))
                    .collect();
                let deps: Vec<&str> = deps.iter().map(|s| s.as_str()).collect();
                svc(&format!("s{i}"), &deps)
            })
            .collect();
        assert_eq!(resolve(&svcset).unwrap(), resolve(&svcset).unwrap());
    }
}

#[cfg(test)]
mod property_tests {
    use super::*;
    use crate::config::RestartPolicy;
    use proptest::prelude::*;

    fn svc(name: String, after: Vec<String>) -> ServiceSpec {
        ServiceSpec {
            name,
            unit: None,
            exec: Some(vec!["/bin/true".into()]),
            after,
            restart: RestartPolicy::Always,
            ready_gate: false,
        }
    }

    /// Random DAG: nodes 0..n, edges j -> i only when j < i (acyclic by
    /// construction, but shuffled so the resolver cannot exploit order).
    #[derive(Debug, Clone)]
    struct Dag {
        n: u32,
        edges: Vec<(u32, u32)>, // (dep, dependent)
        order: Vec<u32>,        // presentation shuffle
    }

    fn dag_strategy() -> impl Strategy<Value = Dag> {
        (
            2u32..24u32,
            proptest::collection::vec((any::<u32>(), any::<u8>()), 0..60),
        )
            .prop_map(|(n, raw)| {
                let edges: Vec<(u32, u32)> = raw
                    .iter()
                    .map(|(r, dens)| {
                        // dependent i in 1..n, dep j in 0..i chosen from the
                        // random bits, present with density.
                        let i = 1 + r % (n - 1);
                        let j = ((*dens as u64 * 2654435761) % i as u64) as u32;
                        (j, i)
                    })
                    .collect();
                let mut order: Vec<u32> = (0..n).collect();
                // deterministic shuffle from the edges' entropy
                for k in 0..order.len() {
                    if let Some((r, _)) = raw.get(k % raw.len().max(1)) {
                        let target = (*r as usize) % order.len();
                        order.swap(k, target);
                    }
                }
                let mut dedup: Vec<(u32, u32)> = edges;
                dedup.sort();
                dedup.dedup();
                Dag {
                    n,
                    edges: dedup,
                    order,
                }
            })
    }

    proptest! {
        #[test]
        fn plan_respects_all_edges(dag in dag_strategy()) {
            let services: Vec<ServiceSpec> = dag.order.iter().map(|i| {
                let after: Vec<String> = dag.edges.iter()
                    .filter(|(_, dep)| dep == i)
                    .map(|(d, _)| format!("n{d}"))
                    .collect();
                svc(format!("n{i}"), after)
            }).collect();
            let names: Vec<String> = (0..dag.n).map(|i| format!("n{i}")).collect();

            let plan = resolve(&services).expect("acyclic input resolves");
            let flat = plan.flat();

            // every service appears exactly once
            let mut sorted = flat.clone();
            sorted.sort();
            let mut want = names.clone();
            want.sort();
            prop_assert_eq!(sorted, want);

            // every edge: dep strictly in an earlier level than dependent
            for (d, i) in &dag.edges {
                let ld = plan.level_of(&format!("n{d}")).unwrap();
                let li = plan.level_of(&format!("n{i}")).unwrap();
                prop_assert!(ld < li, "n{d} (level {ld}) must start before n{i} (level {li})");
            }

            // levels are name-sorted and non-empty
            for lvl in &plan.levels {
                prop_assert!(!lvl.is_empty());
                let mut l = lvl.clone();
                l.sort();
                prop_assert_eq!(&l, lvl);
            }

            // stop order is the exact reverse of start order
            let stop = plan.stop_order();
            let mut flat_rev = flat.clone();
            flat_rev.reverse();
            prop_assert_eq!(stop, flat_rev);
        }
    }

    proptest! {
        #[test]
        fn cyclic_graph_fails(n in 2u32..12u32, back in 0u32..12u32) {
            // Ring: each node depends on the previous one, plus a back edge
            // closing the cycle. Always cyclic.
            let services: Vec<ServiceSpec> = (0..n).map(|i| {
                let prev = if i == 0 { n - 1 } else { i - 1 };
                svc(format!("n{i}"), vec![format!("n{prev}")])
            }).collect();
            let _ = back;
            prop_assert!(resolve(&services).is_err());
        }
    }
}
