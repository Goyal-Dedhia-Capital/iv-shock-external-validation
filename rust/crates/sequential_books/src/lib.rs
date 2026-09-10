//! Pure research scheduling/selection. No prices, costs, fills or account state.
//! All minute timestamps are UTC epoch minutes, never minute-of-day.
use backtest_contracts::{CONTRACT_VERSION, IntentAction, IntentLeg, Side, TradeIntent};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Candidate {
    pub detector: String,
    pub session_date: String,
    pub contract_id: String,
    pub event_minute: i64,
    pub expiry: String,
    pub side: String,
    pub raw_sign: i8,
    pub surface: String,
    pub source_rank: u32,
    pub source_dte: i32,
    #[serde(default)]
    pub source_log_moneyness: f64,
    #[serde(default)]
    pub formation_lane: String,
    #[serde(default)]
    pub recipient_relative_bin: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FamilySpec {
    pub id: &'static str,
    pub direction: Direction,
    pub recipient_side: &'static str,
    pub source_side: &'static str,
    pub raw_sign: i8,
    pub surface: &'static str,
    pub dte_min: i32,
    pub dte_max: i32,
    pub hold_minutes: u32,
    pub formation_lane: Option<&'static str>,
    pub recipient_relative_bin: Option<&'static str>,
}

pub const FAMILIES: [FamilySpec; 10] = [
    FamilySpec {
        id: "H3_F1",
        direction: Direction::Short,
        recipient_side: "CE",
        source_side: "CE",
        raw_sign: 1,
        surface: "coherent",
        dte_min: 0,
        dte_max: 30,
        hold_minutes: 120,
        formation_lane: Some("observed"),
        recipient_relative_bin: Some("observed|positive"),
    },
    FamilySpec {
        id: "H3_F2",
        direction: Direction::Short,
        recipient_side: "CE",
        source_side: "CE",
        raw_sign: 1,
        surface: "coherent",
        dte_min: 31,
        dte_max: 60,
        hold_minutes: 120,
        formation_lane: Some("observed"),
        recipient_relative_bin: Some("observed|positive"),
    },
    FamilySpec {
        id: "H3_F3",
        direction: Direction::Short,
        recipient_side: "CE",
        source_side: "CE",
        raw_sign: 1,
        surface: "coherent",
        dte_min: 31,
        dte_max: 60,
        hold_minutes: 120,
        formation_lane: Some("pchip"),
        recipient_relative_bin: Some("pchip|positive"),
    },
    FamilySpec {
        id: "H3_F4",
        direction: Direction::Short,
        recipient_side: "CE",
        source_side: "CE",
        raw_sign: 1,
        surface: "coherent",
        dte_min: 0,
        dte_max: 30,
        hold_minutes: 120,
        formation_lane: Some("pchip"),
        recipient_relative_bin: Some("pchip|positive"),
    },
    FamilySpec {
        id: "H3_F5",
        direction: Direction::Short,
        recipient_side: "CE",
        source_side: "CE",
        raw_sign: -1,
        surface: "coherent",
        dte_min: 0,
        dte_max: 30,
        hold_minutes: 120,
        formation_lane: Some("pchip"),
        recipient_relative_bin: Some("pchip|negative"),
    },
    FamilySpec {
        id: "H5_F1",
        direction: Direction::Long,
        recipient_side: "PE",
        source_side: "CE",
        raw_sign: 1,
        surface: "coherent",
        dte_min: 31,
        dte_max: 60,
        hold_minutes: 120,
        formation_lane: None,
        recipient_relative_bin: None,
    },
    FamilySpec {
        id: "H5_F2",
        direction: Direction::Long,
        recipient_side: "PE",
        source_side: "CE",
        raw_sign: 1,
        surface: "coherent",
        dte_min: 0,
        dte_max: 30,
        hold_minutes: 60,
        formation_lane: None,
        recipient_relative_bin: None,
    },
    FamilySpec {
        id: "H5_F3",
        direction: Direction::Long,
        recipient_side: "PE",
        source_side: "PE",
        raw_sign: -1,
        surface: "coherent",
        dte_min: 0,
        dte_max: 30,
        hold_minutes: 60,
        formation_lane: None,
        recipient_relative_bin: None,
    },
    FamilySpec {
        id: "H5_F4",
        direction: Direction::Long,
        recipient_side: "PE",
        source_side: "PE",
        raw_sign: -1,
        surface: "coherent",
        dte_min: 31,
        dte_max: 60,
        hold_minutes: 120,
        formation_lane: None,
        recipient_relative_bin: None,
    },
    FamilySpec {
        id: "H5_F5",
        direction: Direction::Long,
        recipient_side: "PE",
        source_side: "PE",
        raw_sign: 1,
        surface: "idiosyncratic",
        dte_min: 31,
        dte_max: 60,
        hold_minutes: 60,
        formation_lane: None,
        recipient_relative_bin: None,
    },
];

/// Return all frozen families matched by an entry-known source candidate.
#[must_use]
pub fn matching_families(candidate: &Candidate) -> Vec<&'static FamilySpec> {
    FAMILIES
        .iter()
        .filter(|family| {
            let source_side_matches = candidate.side == family.source_side;
            let sign_matches = candidate.raw_sign == family.raw_sign;
            let surface_matches = candidate.surface == family.surface;
            let dte_matches = (family.dte_min..=family.dte_max).contains(&candidate.source_dte);
            let formation_matches = family
                .formation_lane
                .is_none_or(|value| candidate.formation_lane == value);
            let relative_bin_matches = family
                .recipient_relative_bin
                .is_none_or(|value| candidate.recipient_relative_bin == value);
            let source_scope_matches = candidate.source_rank >= 3
                && candidate.source_log_moneyness.is_finite()
                && candidate.source_log_moneyness.abs() <= 0.01;
            source_scope_matches
                && source_side_matches
                && sign_matches
                && surface_matches
                && dte_matches
                && formation_matches
                && relative_bin_matches
        })
        .collect()
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Scheduled {
    pub candidate: Candidate,
    pub horizon: u32,
    pub entry_minute: i64,
    pub exit_minute: i64,
    pub frozen_kept: bool,
    pub source_in_scope: bool,
}
/// Input contains post-quiet-gap candidates from the sequential detector.
///
/// All source contracts/signs/families participate before scope is evaluated.
/// Build the sign-pooled per-contract/horizon frozen source schedule.
///
/// # Errors
///
/// Returns an error for duplicate candidate identities, invalid horizons, or
/// minute arithmetic overflow.
pub fn schedule_sources(
    mut rows: Vec<Candidate>,
    horizons: &[u32],
) -> Result<Vec<Scheduled>, String> {
    rows.sort_by(|a, b| {
        (&a.detector, &a.session_date, &a.contract_id, a.event_minute).cmp(&(
            &b.detector,
            &b.session_date,
            &b.contract_id,
            b.event_minute,
        ))
    });
    let mut keys = BTreeSet::new();
    let mut output = Vec::new();
    if horizons.contains(&0) || horizons.iter().collect::<BTreeSet<_>>().len() != horizons.len() {
        return Err("positive unique horizons required".into());
    }
    let mut ends = BTreeMap::new();
    for c in rows {
        if !keys.insert((
            c.detector.clone(),
            c.session_date.clone(),
            c.contract_id.clone(),
            c.event_minute,
        )) {
            return Err("duplicate detector candidate key".into());
        }
        for h in horizons {
            let entry = c.event_minute.checked_add(1).ok_or("minute overflow")?;
            let exit = entry.checked_add(i64::from(*h)).ok_or("minute overflow")?;
            let key = (
                c.detector.clone(),
                c.session_date.clone(),
                c.contract_id.clone(),
                *h,
            );
            let keep = ends.get(&key).is_none_or(|last| entry >= *last);
            if keep {
                ends.insert(key, exit);
            }
            let scope = c.source_rank >= 3
                && (0..=60).contains(&c.source_dte)
                && c.source_log_moneyness.is_finite()
                && c.source_log_moneyness.abs() <= 0.01
                && matches!(c.surface.as_str(), "coherent" | "idiosyncratic")
                && c.raw_sign != 0;
            output.push(Scheduled {
                candidate: c.clone(),
                horizon: *h,
                entry_minute: entry,
                exit_minute: exit,
                frozen_kept: keep,
                source_in_scope: scope,
            });
        }
    }
    Ok(output)
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Long,
    Short,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Overlap {
    RecipientContract,
    Underlying,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Selector {
    pub id: String,
    pub side: String,
    pub money_bin: String,
    pub dte_bin: String,
    pub target_logm: f64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Recipient {
    pub contract_id: String,
    pub expiry: String,
    pub expiry_minute: i64,
    pub side: String,
    pub money_bin: String,
    pub dte_bin: String,
    pub logm: f64,
    pub lot_size: Option<u64>,
    pub lot_status: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceUnit {
    pub id: String,
    pub detector: String,
    pub session_date: String,
    pub event_minute: i64,
    pub expiry: String,
    pub side: String,
    pub horizon: u32,
    pub session_end_minute: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Admission {
    pub book_id: String,
    pub source_unit_id: String,
    pub recipient_contract_id: Option<String>,
    pub entry_minute: i64,
    pub exit_minute: i64,
    pub admitted: bool,
    pub reason: String,
    pub intent: Option<TradeIntent>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Books {
    reserved_until: BTreeMap<String, i64>,
    last_minute: Option<i64>,
}
impl Books {
    /// Complete one strictly chronological event-minute batch.
    ///
    /// # Errors
    ///
    /// Returns an error for non-monotonic batches, duplicate source/recipient
    /// identities, invalid selectors, or minute arithmetic overflow. Validation
    /// completes before any reservation state changes.
    pub fn on_minute(
        &mut self,
        mut sources: Vec<SourceUnit>,
        recipients: &[Recipient],
        selectors: &[Selector],
    ) -> Result<Vec<Admission>, String> {
        if sources.is_empty() {
            return Ok(Vec::new());
        }
        let t = validate_minute_batch(self.last_minute, &sources, recipients, selectors)?;
        let entry = t.checked_add(1).ok_or("minute overflow")?;
        self.last_minute = Some(t);
        sources.sort_by(|a, b| {
            (&a.expiry, &a.side, &a.id, &a.detector, a.horizon).cmp(&(
                &b.expiry,
                &b.side,
                &b.id,
                &b.detector,
                b.horizon,
            ))
        });

        let mut output = Vec::new();
        for source in &sources {
            let exit = entry
                .checked_add(i64::from(source.horizon))
                .ok_or("minute overflow")?;
            for selector in selectors {
                let recipient = select_recipient(recipients, selector, exit);
                for direction in [Direction::Long, Direction::Short] {
                    for policy in [Overlap::RecipientContract, Overlap::Underlying] {
                        output.push(self.admit(source, selector, recipient, &direction, &policy));
                    }
                }
            }
        }
        self.reserved_until.retain(|_, end| *end > t);
        Ok(output)
    }

    fn admit(
        &mut self,
        source: &SourceUnit,
        selector: &Selector,
        recipient: Option<&Recipient>,
        direction: &Direction,
        policy: &Overlap,
    ) -> Admission {
        let entry = source
            .event_minute
            .checked_add(1)
            .expect("minute arithmetic validated before mutation");
        let exit = entry
            .checked_add(i64::from(source.horizon))
            .expect("minute arithmetic validated before mutation");
        let book = format!(
            "{}|{direction:?}|{}|{}|{policy:?}",
            source.detector, source.horizon, selector.id
        );
        let mut admission = Admission {
            book_id: book.clone(),
            source_unit_id: source.id.clone(),
            recipient_contract_id: recipient.map(|value| value.contract_id.clone()),
            entry_minute: entry,
            exit_minute: exit,
            admitted: false,
            reason: String::new(),
            intent: None,
        };
        if source.horizon == 0 || exit > source.session_end_minute {
            admission.reason = "planned_session_end_ineligible".into();
            return admission;
        }
        let Some(recipient) = recipient else {
            admission.reason = "no_event_time_eligible_recipient".into();
            return admission;
        };
        let occupancy = match policy {
            Overlap::RecipientContract => format!("{book}|{}", recipient.contract_id),
            Overlap::Underlying => book.clone(),
        };
        if self
            .reserved_until
            .get(&occupancy)
            .is_some_and(|reserved_exit| entry < *reserved_exit)
        {
            admission.reason = "planned_overlap_conflict".into();
            return admission;
        }

        // Reservation precedes quantity and endpoint availability. Same-minute
        // closes occur before opens, so entry == prior exit is available.
        self.reserved_until.insert(occupancy, exit);
        admission.admitted = true;
        let Some(quantity) = recipient.lot_size.filter(|quantity| *quantity > 0) else {
            admission.reason = "planned_admission_quantity_unresolved".into();
            return admission;
        };
        let id = format!("{book}|{}|{}", source.id, recipient.contract_id);
        admission.intent = Some(TradeIntent {
            schema_version: CONTRACT_VERSION.into(),
            intent_id: format!("{id}|open"),
            decision_id: source.id.clone(),
            strategy_position_id: id,
            basket_key: recipient.contract_id.clone(),
            action: IntentAction::Open,
            atomic: true,
            legs: vec![IntentLeg {
                instrument_id: recipient.contract_id.clone(),
                side: if *direction == Direction::Long {
                    Side::Buy
                } else {
                    Side::Sell
                },
                quantity,
                limit_price: None,
            }],
            lineage: BTreeMap::from([
                ("book_id".into(), book),
                ("source_unit_id".into(), source.id.clone()),
                ("entry_minute".into(), entry.to_string()),
                ("exit_minute".into(), exit.to_string()),
                ("lot_status".into(), recipient.lot_status.clone()),
                ("execution_mode".into(), "native_close_modeled_v1".into()),
            ]),
        });
        admission.reason = "planned_admission".into();
        admission
    }
}

fn validate_minute_batch(
    last_minute: Option<i64>,
    sources: &[SourceUnit],
    recipients: &[Recipient],
    selectors: &[Selector],
) -> Result<i64, String> {
    let t = sources[0].event_minute;
    if last_minute.is_some_and(|value| t <= value)
        || sources.iter().any(|source| source.event_minute != t)
    {
        return Err("strictly chronological complete minute batches required".into());
    }
    let mut seen = BTreeSet::new();
    if sources
        .iter()
        .any(|source| !seen.insert((&source.id, source.horizon, &source.detector)))
    {
        return Err("duplicate source unit/horizon/detector".into());
    }
    let mut recipient_ids = BTreeSet::new();
    if recipients
        .iter()
        .any(|recipient| !recipient_ids.insert(&recipient.contract_id))
    {
        return Err("duplicate event-time recipient".into());
    }
    let mut selector_ids = BTreeSet::new();
    if selectors
        .iter()
        .any(|selector| !selector.target_logm.is_finite() || !selector_ids.insert(&selector.id))
    {
        return Err("finite unique selectors required".into());
    }
    let entry = t.checked_add(1).ok_or("minute overflow")?;
    for source in sources {
        entry
            .checked_add(i64::from(source.horizon))
            .ok_or("minute overflow")?;
    }
    Ok(t)
}

fn select_recipient<'a>(
    recipients: &'a [Recipient],
    selector: &Selector,
    exit: i64,
) -> Option<&'a Recipient> {
    recipients
        .iter()
        .filter(|recipient| {
            recipient.side == selector.side
                && recipient.money_bin == selector.money_bin
                && recipient.dte_bin == selector.dte_bin
                && recipient.logm.is_finite()
                && recipient.expiry_minute >= exit
        })
        .min_by(|left, right| {
            left.expiry
                .cmp(&right.expiry)
                .then_with(|| {
                    (left.logm - selector.target_logm)
                        .abs()
                        .total_cmp(&(right.logm - selector.target_logm).abs())
                })
                .then_with(|| left.contract_id.cmp(&right.contract_id))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn candidate(t: i64, rank: u32) -> Candidate {
        Candidate {
            detector: "S0".into(),
            session_date: "2024-01-01".into(),
            contract_id: "a".into(),
            event_minute: t,
            expiry: "2024-02-01".into(),
            side: "CE".into(),
            raw_sign: 1,
            surface: "coherent".into(),
            source_rank: rank,
            source_dte: 30,
            source_log_moneyness: 0.0,
            formation_lane: "observed".into(),
            recipient_relative_bin: "observed|positive".into(),
        }
    }
    #[test]
    fn frozen_schedule_is_before_scope_and_allows_equal_entry_exit() {
        let x = schedule_sources(
            vec![candidate(0, 1), candidate(5, 3), candidate(60, 3)],
            &[60],
        )
        .unwrap();
        assert!(x[0].frozen_kept && !x[0].source_in_scope);
        assert!(!x[1].frozen_kept);
        assert!(x[2].frozen_kept);
    }
    #[test]
    fn frozen_family_registry_matches_all_ten_definitions() {
        let cases = [
            (
                "H3_F1",
                "CE",
                1,
                "coherent",
                0,
                "observed",
                "observed|positive",
            ),
            (
                "H3_F2",
                "CE",
                1,
                "coherent",
                31,
                "observed",
                "observed|positive",
            ),
            ("H3_F3", "CE", 1, "coherent", 31, "pchip", "pchip|positive"),
            ("H3_F4", "CE", 1, "coherent", 0, "pchip", "pchip|positive"),
            ("H3_F5", "CE", -1, "coherent", 0, "pchip", "pchip|negative"),
            (
                "H5_F1",
                "CE",
                1,
                "coherent",
                60,
                "unsupported",
                "unsupported",
            ),
            (
                "H5_F2",
                "CE",
                1,
                "coherent",
                30,
                "unsupported",
                "unsupported",
            ),
            (
                "H5_F3",
                "PE",
                -1,
                "coherent",
                0,
                "unsupported",
                "unsupported",
            ),
            (
                "H5_F4",
                "PE",
                -1,
                "coherent",
                31,
                "unsupported",
                "unsupported",
            ),
            (
                "H5_F5",
                "PE",
                1,
                "idiosyncratic",
                60,
                "unsupported",
                "unsupported",
            ),
        ];
        for (expected, side, sign, surface, dte, lane, relative) in cases {
            let mut row = candidate(0, 3);
            row.side = side.into();
            row.raw_sign = sign;
            row.surface = surface.into();
            row.source_dte = dte;
            row.formation_lane = lane.into();
            row.recipient_relative_bin = relative.into();
            assert!(
                matching_families(&row)
                    .iter()
                    .any(|family| family.id == expected),
                "{expected} did not match its boundary fixture"
            );
        }
        assert_eq!(FAMILIES.len(), 10);
        assert_eq!(
            FAMILIES
                .iter()
                .filter(|f| f.direction == Direction::Short)
                .count(),
            5
        );
        assert_eq!(
            FAMILIES
                .iter()
                .filter(|f| f.direction == Direction::Long)
                .count(),
            5
        );
    }

    #[test]
    fn family_scope_enforces_rank_near_atm_and_inclusive_dte_edges() {
        let mut row = candidate(0, 2);
        assert!(matching_families(&row).is_empty());
        row.source_rank = 3;
        row.source_log_moneyness = 0.01;
        assert!(matching_families(&row).iter().any(|f| f.id == "H3_F1"));
        row.source_log_moneyness = -0.01;
        assert!(matching_families(&row).iter().any(|f| f.id == "H3_F1"));
        row.source_log_moneyness = 0.010_000_1;
        assert!(matching_families(&row).is_empty());
        row.source_log_moneyness = 0.0;
        row.source_dte = 61;
        assert!(matching_families(&row).is_empty());
    }
    fn source(t: i64, id: &str) -> SourceUnit {
        SourceUnit {
            id: id.into(),
            detector: "S0".into(),
            session_date: "2024-01-01".into(),
            event_minute: t,
            expiry: "2024-02-01".into(),
            side: "CE".into(),
            horizon: 60,
            session_end_minute: 400,
        }
    }
    fn recipient() -> Recipient {
        Recipient {
            contract_id: "a".into(),
            expiry: "2024-02-01".into(),
            expiry_minute: 10000,
            side: "PE".into(),
            money_bin: "atm".into(),
            dte_bin: "31-60".into(),
            logm: 0.,
            lot_size: Some(25),
            lot_status: "exact".into(),
        }
    }
    fn selector() -> Selector {
        Selector {
            id: "pe-atm-31-60".into(),
            side: "PE".into(),
            money_bin: "atm".into(),
            dte_bin: "31-60".into(),
            target_logm: 0.,
        }
    }
    #[test]
    fn overlay_is_strict_and_no_fill_cannot_reclaim_reservation() {
        let mut b = Books::default();
        let mut r = recipient();
        r.lot_size = None;
        let first = b
            .on_minute(vec![source(0, "a")], &[r], &[selector()])
            .unwrap();
        assert!(first.iter().all(|a| a.admitted && a.intent.is_none()));
        let equal = b
            .on_minute(vec![source(60, "b")], &[recipient()], &[selector()])
            .unwrap();
        assert!(equal.iter().all(|a| a.admitted && a.intent.is_some()));
        let later = b
            .on_minute(vec![source(61, "c")], &[recipient()], &[selector()])
            .unwrap();
        assert!(later.iter().all(|a| !a.admitted));
    }
    #[test]
    fn simultaneous_sources_and_expiry_selection_do_not_multiply_orders() {
        let mut b = Books::default();
        let mut farther = recipient();
        farther.contract_id = "b".into();
        farther.expiry = "2024-03-01".into();
        let out = b
            .on_minute(
                vec![source(0, "b"), source(0, "a")],
                &[farther, recipient()],
                &[selector()],
            )
            .unwrap();
        assert_eq!(out.iter().filter(|a| a.admitted).count(), 4);
        assert!(
            out.iter()
                .all(|a| a.recipient_contract_id.as_deref() == Some("a"))
        );
        assert!(
            out.iter()
                .filter(|a| a.admitted)
                .all(|a| a.source_unit_id == "a")
        );
    }
    #[test]
    fn invalid_batch_does_not_consume_clock() {
        let mut b = Books::default();
        assert!(
            b.on_minute(
                vec![source(1, "a"), source(1, "a")],
                &[recipient()],
                &[selector()]
            )
            .is_err()
        );
        assert!(
            b.on_minute(vec![source(1, "a")], &[recipient()], &[selector()])
                .unwrap()
                .iter()
                .all(|a| a.admitted)
        );
    }
}
