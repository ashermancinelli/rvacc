//! Deterministic, single-threaded discrete-event simulation.
//!
//! Events are ordered by `(cycle, insertion sequence)`. Handlers return effects;
//! the simulator validates and queues those effects after the handler returns.
//! Zero-delay delivery is allowed and follows already queued events at that cycle.
//! Every run has an event budget, so zero-delay loops remain interruptible.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::fmt;

pub type Cycle = u64;

/// An index local to one simulator.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct ComponentId(usize);

/// Port numbers are defined by each component, not registered with the simulator.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PortId(pub u32);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct Endpoint {
    pub component: ComponentId,
    pub port: PortId,
}

/// An outgoing message, delayed by both `after` and the connected route latency.
#[derive(Debug)]
pub struct Effect<M> {
    pub port: PortId,
    pub after: Cycle,
    pub message: M,
}

pub trait Component<M> {
    fn on_event(
        &mut self,
        cycle: Cycle,
        port: PortId,
        message: M,
    ) -> Result<Vec<Effect<M>>, String>;

    /// False when work remains, including work waiting on another component.
    fn is_idle(&self) -> bool {
        true
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SimError {
    UnknownComponent(ComponentId),
    UnconnectedPort(Endpoint),
    AlreadyConnected(Endpoint),
    PastEvent {
        now: Cycle,
        requested: Cycle,
    },
    CycleOverflow,
    SequenceOverflow,
    Handler {
        component: ComponentId,
        reason: String,
    },
}

impl fmt::Display for SimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}

impl std::error::Error for SimError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RunOutcome {
    Complete,
    Deadlock,
    BudgetExhausted,
    Failed(SimError),
}

struct Event<M> {
    cycle: Cycle,
    sequence: u64,
    destination: Endpoint,
    message: M,
}

// Reverse time ordering for BinaryHeap. Message types need no ordering traits.
impl<M> Ord for Event<M> {
    fn cmp(&self, other: &Self) -> Ordering {
        (other.cycle, other.sequence).cmp(&(self.cycle, self.sequence))
    }
}

impl<M> PartialOrd for Event<M> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<M> PartialEq for Event<M> {
    fn eq(&self, other: &Self) -> bool {
        (self.cycle, self.sequence) == (other.cycle, other.sequence)
    }
}

impl<M> Eq for Event<M> {}

#[derive(Clone, Copy)]
struct Route {
    destination: Endpoint,
    latency: Cycle,
}

pub struct Simulator<M> {
    now: Cycle,
    next_sequence: u64,
    events: BinaryHeap<Event<M>>,
    components: Vec<Box<dyn Component<M>>>,
    routes: HashMap<Endpoint, Route>,
    failure: Option<SimError>,
}

impl<M> Default for Simulator<M> {
    fn default() -> Self {
        Self {
            now: 0,
            next_sequence: 0,
            events: BinaryHeap::new(),
            components: Vec::new(),
            routes: HashMap::new(),
            failure: None,
        }
    }
}

impl<M> Simulator<M> {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn now(&self) -> Cycle {
        self.now
    }

    pub fn add_component(&mut self, component: impl Component<M> + 'static) -> ComponentId {
        let id = ComponentId(self.components.len());
        self.components.push(Box::new(component));
        id
    }

    fn validate(&self, id: ComponentId) -> Result<(), SimError> {
        if id.0 < self.components.len() {
            Ok(())
        } else {
            Err(SimError::UnknownComponent(id))
        }
    }

    /// Connect one output port to one input endpoint. Fan-out is explicit in effects.
    pub fn connect(
        &mut self,
        source: Endpoint,
        destination: Endpoint,
        latency: Cycle,
    ) -> Result<(), SimError> {
        self.validate(source.component)?;
        self.validate(destination.component)?;
        if self.routes.contains_key(&source) {
            return Err(SimError::AlreadyConnected(source));
        }
        self.routes.insert(
            source,
            Route {
                destination,
                latency,
            },
        );
        Ok(())
    }

    /// Inject an event at an absolute cycle. Rejected events do not change the queue.
    pub fn schedule(
        &mut self,
        cycle: Cycle,
        destination: Endpoint,
        message: M,
    ) -> Result<(), SimError> {
        if let Some(error) = &self.failure {
            return Err(error.clone());
        }
        self.validate(destination.component)?;
        if cycle < self.now {
            return Err(SimError::PastEvent {
                now: self.now,
                requested: cycle,
            });
        }
        let next = self
            .next_sequence
            .checked_add(1)
            .ok_or(SimError::SequenceOverflow)?;
        self.events.push(Event {
            cycle,
            sequence: self.next_sequence,
            destination,
            message,
        });
        self.next_sequence = next;
        Ok(())
    }

    fn dispatch(&mut self, event: Event<M>) -> Result<(), SimError> {
        self.now = event.cycle;
        let source = event.destination.component;
        let effects = self.components[source.0]
            .on_event(self.now, event.destination.port, event.message)
            .map_err(|reason| SimError::Handler {
                component: source,
                reason,
            })?;

        // Validate the whole batch before queuing any effects. Component state is
        // not rolled back on failure; a failed simulator cannot run again.
        let count = u64::try_from(effects.len()).map_err(|_| SimError::SequenceOverflow)?;
        self.next_sequence
            .checked_add(count)
            .ok_or(SimError::SequenceOverflow)?;
        let mut pending = Vec::with_capacity(effects.len());
        for effect in effects {
            let endpoint = Endpoint {
                component: source,
                port: effect.port,
            };
            let route = self
                .routes
                .get(&endpoint)
                .ok_or(SimError::UnconnectedPort(endpoint))?;
            let cycle = self
                .now
                .checked_add(effect.after)
                .and_then(|cycle| cycle.checked_add(route.latency))
                .ok_or(SimError::CycleOverflow)?;
            pending.push((cycle, route.destination, effect.message));
        }
        for (cycle, destination, message) in pending {
            self.schedule(cycle, destination, message)?;
        }
        Ok(())
    }

    /// Process at most `event_budget` events, jumping directly to each event cycle.
    /// Empty-queue status takes precedence over budget exhaustion. Deadlock can
    /// be resumed after external event injection; failures are terminal.
    pub fn run(&mut self, event_budget: usize) -> RunOutcome {
        if let Some(error) = &self.failure {
            return RunOutcome::Failed(error.clone());
        }
        for _ in 0..event_budget {
            let Some(event) = self.events.pop() else {
                break;
            };
            if let Err(error) = self.dispatch(event) {
                self.failure = Some(error.clone());
                return RunOutcome::Failed(error);
            }
        }
        if !self.events.is_empty() {
            RunOutcome::BudgetExhausted
        } else if self.components.iter().all(|component| component.is_idle()) {
            RunOutcome::Complete
        } else {
            RunOutcome::Deadlock
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    const PORT: PortId = PortId(0);
    type Log = Rc<RefCell<Vec<(Cycle, u32)>>>;

    struct Recorder(Log);
    impl Component<u32> for Recorder {
        fn on_event(
            &mut self,
            cycle: Cycle,
            _: PortId,
            value: u32,
        ) -> Result<Vec<Effect<u32>>, String> {
            self.0.borrow_mut().push((cycle, value));
            Ok(Vec::new())
        }
    }

    struct Relay {
        delay: Cycle,
    }
    impl Component<u32> for Relay {
        fn on_event(
            &mut self,
            _: Cycle,
            _: PortId,
            value: u32,
        ) -> Result<Vec<Effect<u32>>, String> {
            Ok(vec![Effect {
                port: PORT,
                after: self.delay,
                message: value,
            }])
        }
    }

    fn endpoint(component: ComponentId) -> Endpoint {
        Endpoint {
            component,
            port: PORT,
        }
    }

    #[test]
    fn mock_memory_round_trip_and_resume() {
        let mut sim = Simulator::new();
        let log = Log::default();
        let host = sim.add_component(Recorder(log.clone()));
        let request = sim.add_component(Relay { delay: 0 });
        let memory = sim.add_component(Relay { delay: 7 });
        sim.connect(endpoint(request), endpoint(memory), 2).unwrap();
        sim.connect(endpoint(memory), endpoint(host), 3).unwrap();
        sim.schedule(5, endpoint(request), 42).unwrap();
        assert_eq!(sim.run(0), RunOutcome::BudgetExhausted);
        assert_eq!(sim.now(), 0);
        assert_eq!(sim.run(1), RunOutcome::BudgetExhausted);
        assert_eq!(sim.now(), 5);
        assert!(log.borrow().is_empty());
        assert_eq!(sim.run(1), RunOutcome::BudgetExhausted);
        assert_eq!(sim.now(), 7);
        assert_eq!(sim.run(1), RunOutcome::Complete);
        // Start 5 + request transport 2 + memory access 7 + response transport 3.
        assert_eq!(*log.borrow(), vec![(17, 42)]);
    }

    #[test]
    fn equal_cycle_events_keep_insertion_order() {
        let mut sim = Simulator::new();
        let log = Log::default();
        let sink = sim.add_component(Recorder(log.clone()));
        let relay = sim.add_component(Relay { delay: 0 });
        sim.connect(endpoint(relay), endpoint(sink), 0).unwrap();
        sim.schedule(10, endpoint(sink), 4).unwrap();
        sim.schedule(2, endpoint(relay), 1).unwrap();
        sim.schedule(2, endpoint(sink), 2).unwrap();
        sim.schedule(2, endpoint(sink), 3).unwrap();
        assert_eq!(sim.run(10), RunOutcome::Complete);
        assert_eq!(*log.borrow(), vec![(2, 2), (2, 3), (2, 1), (10, 4)]);
        assert_eq!(
            sim.schedule(9, endpoint(sink), 0),
            Err(SimError::PastEvent {
                now: 10,
                requested: 9
            })
        );
        assert_eq!(sim.run(0), RunOutcome::Complete);
    }

    #[test]
    fn zero_delay_loop_is_resumable() {
        let mut sim = Simulator::new();
        let relay = sim.add_component(Relay { delay: 0 });
        sim.connect(endpoint(relay), endpoint(relay), 0).unwrap();
        sim.schedule(0, endpoint(relay), 0).unwrap();
        assert_eq!(sim.run(100), RunOutcome::BudgetExhausted);
        assert_eq!(sim.run(100), RunOutcome::BudgetExhausted);
        assert_eq!(sim.now(), 0);
    }

    #[test]
    fn deadlock_can_be_resumed_by_external_input() {
        struct Waiting(bool);
        impl Component<()> for Waiting {
            fn on_event(&mut self, _: Cycle, _: PortId, _: ()) -> Result<Vec<Effect<()>>, String> {
                self.0 = false;
                Ok(vec![])
            }
            fn is_idle(&self) -> bool {
                !self.0
            }
        }
        let mut sim = Simulator::new();
        let waiting = sim.add_component(Waiting(true));
        assert_eq!(sim.run(10), RunOutcome::Deadlock);
        sim.schedule(9, endpoint(waiting), ()).unwrap();
        assert_eq!(sim.run(1), RunOutcome::Complete);
    }

    #[test]
    fn invalid_routes_and_overflow_are_errors() {
        let mut sim = Simulator::new();
        let relay = sim.add_component(Relay { delay: 1 });
        let bad = endpoint(ComponentId(99));
        assert_eq!(
            sim.schedule(0, bad, 0),
            Err(SimError::UnknownComponent(bad.component))
        );
        assert_eq!(
            sim.connect(endpoint(relay), bad, 0),
            Err(SimError::UnknownComponent(bad.component))
        );
        sim.connect(endpoint(relay), endpoint(relay), 1).unwrap();
        assert_eq!(
            sim.connect(endpoint(relay), endpoint(relay), 0),
            Err(SimError::AlreadyConnected(endpoint(relay)))
        );
        sim.schedule(Cycle::MAX - 1, endpoint(relay), 0).unwrap();
        assert_eq!(sim.run(1), RunOutcome::Failed(SimError::CycleOverflow));
        assert_eq!(sim.run(1), RunOutcome::Failed(SimError::CycleOverflow));
        assert_eq!(
            sim.schedule(Cycle::MAX, endpoint(relay), 0),
            Err(SimError::CycleOverflow)
        );
    }

    #[test]
    fn failed_batch_queues_no_effects() {
        struct Broken;
        impl Component<u32> for Broken {
            fn on_event(
                &mut self,
                _: Cycle,
                _: PortId,
                _: u32,
            ) -> Result<Vec<Effect<u32>>, String> {
                Ok(vec![
                    Effect {
                        port: PORT,
                        after: 0,
                        message: 1,
                    },
                    Effect {
                        port: PortId(1),
                        after: 0,
                        message: 2,
                    },
                ])
            }
        }
        let mut sim = Simulator::new();
        let broken = sim.add_component(Broken);
        sim.connect(endpoint(broken), endpoint(broken), 0).unwrap();
        sim.schedule(0, endpoint(broken), 0).unwrap();
        assert_eq!(
            sim.run(1),
            RunOutcome::Failed(SimError::UnconnectedPort(Endpoint {
                component: broken,
                port: PortId(1)
            }))
        );
        assert!(sim.events.is_empty());
    }

    #[test]
    fn handler_failure_is_terminal() {
        struct Broken;
        impl Component<()> for Broken {
            fn on_event(&mut self, _: Cycle, _: PortId, _: ()) -> Result<Vec<Effect<()>>, String> {
                Err("device fault".into())
            }
        }
        let mut sim = Simulator::new();
        let broken = sim.add_component(Broken);
        sim.schedule(3, endpoint(broken), ()).unwrap();
        let expected = RunOutcome::Failed(SimError::Handler {
            component: broken,
            reason: "device fault".into(),
        });
        assert_eq!(sim.run(1), expected);
        assert_eq!(sim.run(1), expected);
    }

    #[test]
    fn sequence_overflow_does_not_queue_an_event() {
        let mut sim = Simulator::new();
        let sink = sim.add_component(Recorder(Log::default()));
        sim.next_sequence = u64::MAX;
        assert_eq!(
            sim.schedule(0, endpoint(sink), 1),
            Err(SimError::SequenceOverflow)
        );
        assert!(sim.events.is_empty());
    }
}
