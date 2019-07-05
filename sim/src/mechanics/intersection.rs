use crate::{AgentID, Command, Scheduler, Speed};
use abstutil::{deserialize_btreemap, serialize_btreemap};
use geom::Duration;
use map_model::{
    ControlStopSign, ControlTrafficSignal, IntersectionID, IntersectionType, LaneID, Map, TurnID,
    TurnPriority,
};
use serde_derive::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, HashSet};

const WAIT_AT_STOP_SIGN: Duration = Duration::const_seconds(0.5);

#[derive(Serialize, Deserialize, PartialEq)]
pub struct IntersectionSimState {
    state: BTreeMap<IntersectionID, State>,
}

#[derive(Serialize, Deserialize, PartialEq)]
struct State {
    id: IntersectionID,
    accepted: BTreeSet<Request>,
    // Track when a request is first made.
    #[serde(
        serialize_with = "serialize_btreemap",
        deserialize_with = "deserialize_btreemap"
    )]
    waiting: BTreeMap<Request, Duration>,
}

impl IntersectionSimState {
    pub fn new(map: &Map, scheduler: &mut Scheduler) -> IntersectionSimState {
        let mut sim = IntersectionSimState {
            state: BTreeMap::new(),
        };
        for i in map.all_intersections() {
            sim.state.insert(
                i.id,
                State {
                    id: i.id,
                    accepted: BTreeSet::new(),
                    waiting: BTreeMap::new(),
                },
            );
            if i.intersection_type == IntersectionType::TrafficSignal {
                sim.update_intersection(Duration::ZERO, i.id, map, scheduler);
            }
        }
        sim
    }

    pub fn nobody_headed_towards(&self, lane: LaneID, i: IntersectionID) -> bool {
        !self.state[&i]
            .accepted
            .iter()
            .any(|req| req.turn.dst == lane)
    }

    pub fn turn_finished(
        &mut self,
        now: Duration,
        agent: AgentID,
        turn: TurnID,
        scheduler: &mut Scheduler,
    ) {
        let state = self.state.get_mut(&turn.parent).unwrap();

        assert!(state.accepted.remove(&Request { agent, turn }));

        // TODO Could be smarter here. For both policies, only wake up agents that would then be
        // accepted. For now, wake up everyone -- for traffic signals, maybe a Yield and Priority
        // finished and could let another one in.

        for req in state.waiting.keys() {
            // TODO Use update because multiple agents could finish a turn at the same time, before
            // the waiting one has a chance to try again.
            scheduler.update(now, Command::update_agent(req.agent));
        }
    }

    // This is only triggered for traffic signals.
    pub fn update_intersection(
        &self,
        now: Duration,
        id: IntersectionID,
        map: &Map,
        scheduler: &mut Scheduler,
    ) {
        let state = &self.state[&id];
        let (_, _, remaining) = map
            .get_traffic_signal(id)
            .current_cycle_and_remaining_time(now);

        // TODO Wake up everyone, for now.
        // TODO Use update in case turn_finished scheduled an event for them already.
        for req in state.waiting.keys() {
            scheduler.update(now, Command::update_agent(req.agent));
        }

        scheduler.push(now + remaining, Command::UpdateIntersection(id));
    }

    // For cars: The head car calls this when they're at the end of the lane WaitingToAdvance. If
    // this returns true, then the head car MUST actually start this turn.
    // For peds: Likewise -- only called when the ped is at the start of the turn. They must
    // actually do the turn if this returns true.
    //
    // If this returns false, the agent should NOT retry. IntersectionSimState will schedule a
    // retry event at some point.
    pub fn maybe_start_turn(
        &mut self,
        agent: AgentID,
        turn: TurnID,
        speed: Speed,
        now: Duration,
        map: &Map,
        scheduler: &mut Scheduler,
    ) -> bool {
        let req = Request { agent, turn };
        let state = self.state.get_mut(&turn.parent).unwrap();
        state.waiting.entry(req.clone()).or_insert(now);

        let allowed = if let Some(ref signal) = map.maybe_get_traffic_signal(state.id) {
            state.traffic_signal_policy(signal, &req, speed, now, map)
        } else if let Some(ref sign) = map.maybe_get_stop_sign(state.id) {
            state.stop_sign_policy(sign, &req, now, map, scheduler)
        } else {
            // TODO This never gets called right now
            state.freeform_policy(&req, map)
        };

        if allowed {
            assert!(!state.any_accepted_conflict_with(turn, map));
            state.waiting.remove(&req).unwrap();
            state.accepted.insert(req);
            true
        } else {
            false
        }
    }

    pub fn debug(&self, id: IntersectionID, map: &Map) {
        println!("{}", abstutil::to_json(&self.state[&id]));
        if let Some(ref sign) = map.maybe_get_stop_sign(id) {
            println!("{}", abstutil::to_json(sign));
        } else if let Some(ref signal) = map.maybe_get_traffic_signal(id) {
            println!("{}", abstutil::to_json(signal));
        } else {
            println!("Border");
        }
    }

    pub fn get_accepted_agents(&self, id: IntersectionID) -> HashSet<AgentID> {
        self.state[&id]
            .accepted
            .iter()
            .map(|req| req.agent)
            .collect()
    }
}

impl State {
    fn any_accepted_conflict_with(&self, t: TurnID, map: &Map) -> bool {
        let turn = map.get_t(t);
        self.accepted
            .iter()
            .any(|req| map.get_t(req.turn).conflicts_with(turn))
    }

    fn freeform_policy(&self, req: &Request, map: &Map) -> bool {
        // Allow concurrent turns that don't conflict, don't prevent target lane from spilling
        // over.
        if self.any_accepted_conflict_with(req.turn, map) {
            return false;
        }
        true
    }

    fn is_ready_at_stop_sign(
        &self,
        sign: &ControlStopSign,
        req: &Request,
        now: Duration,
        map: &Map,
    ) -> bool {
        if self.any_accepted_conflict_with(req.turn, map) {
            return false;
        }

        let our_priority = sign.turns[&req.turn];
        let our_time = self.waiting[req];

        if our_priority == TurnPriority::Stop && now < our_time + WAIT_AT_STOP_SIGN {
            return false;
        }

        true
    }

    fn stop_sign_policy(
        &self,
        sign: &ControlStopSign,
        req: &Request,
        now: Duration,
        map: &Map,
        scheduler: &mut Scheduler,
    ) -> bool {
        if self.any_accepted_conflict_with(req.turn, map) {
            return false;
        }

        let our_priority = sign.turns[&req.turn];
        assert!(our_priority != TurnPriority::Banned);
        let our_time = self.waiting[req];

        if our_priority == TurnPriority::Stop && now < our_time + WAIT_AT_STOP_SIGN {
            // Since we have "ownership" of scheduling for req.agent, don't need to use
            // scheduler.update.
            scheduler.push(
                our_time + WAIT_AT_STOP_SIGN,
                Command::update_agent(req.agent),
            );
            return false;
        }

        let our_turn = map.get_t(req.turn);
        for (r, time) in &self.waiting {
            // If the turns don't conflict, then don't even worry.
            if !our_turn.conflicts_with(map.get_t(r.turn)) {
                continue;
            }
            // If the other can't go yet, then proceed.
            if !self.is_ready_at_stop_sign(sign, r, now, map) {
                continue;
            }

            // If there's a higher rank turn waiting, don't allow
            if sign.turns[&r.turn] > our_priority {
                return false;
            // If there's an equal rank turn queued before ours, don't allow
            } else if sign.turns[&r.turn] == our_priority && *time < our_time {
                return false;
            }
        }

        // TODO Make sure we can optimistically finish this turn before an approaching
        // higher-priority vehicle wants to begin.

        true
    }

    fn traffic_signal_policy(
        &self,
        signal: &ControlTrafficSignal,
        new_req: &Request,
        speed: Speed,
        time: Duration,
        map: &Map,
    ) -> bool {
        let (_, cycle, remaining_cycle_time) = signal.current_cycle_and_remaining_time(time);

        // Can't go at all this cycle.
        if cycle.get_priority(new_req.turn) == TurnPriority::Banned {
            return false;
        }

        // Somebody might already be doing a Yield turn that conflicts with this one.
        if self.any_accepted_conflict_with(new_req.turn, map) {
            return false;
        }

        // A yield loses to a conflicting Priority turn.
        let turn = map.get_t(new_req.turn);
        if cycle.get_priority(new_req.turn) == TurnPriority::Yield {
            if self.waiting.keys().any(|r| {
                turn.conflicts_with(map.get_t(r.turn))
                    && cycle.get_priority(r.turn) == TurnPriority::Priority
            }) {
                return false;
            }
        }

        // TODO Make sure we can optimistically finish this turn before an approaching
        // higher-priority vehicle wants to begin.

        // Optimistically if nobody else is in the way, this is how long it'll take to finish the
        // turn. Don't start the turn if we won't finish by the time the light changes. If we get
        // it wrong, that's fine -- block the box a bit.
        let time_to_cross = turn.geom.length() / speed;
        if time_to_cross > remaining_cycle_time {
            return false;
        }

        true
    }
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, Clone, Debug)]
struct Request {
    agent: AgentID,
    turn: TurnID,
}
