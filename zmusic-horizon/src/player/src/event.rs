use std::collections::VecDeque;

const DEFAULT_CAPACITY: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum Event {
    None = 0,
    StateChanged = 1,
    TrackEnded = 2,
    ProgressUpdate = 3,
    Error = 4,
    Buffering = 5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventOverflow;

#[derive(Debug)]
pub struct EventMailbox {
    events: VecDeque<Event>,
    capacity: usize,
}

impl Default for EventMailbox {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

impl EventMailbox {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            events: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn post(&mut self, event: Event) -> Result<(), EventOverflow> {
        if event == Event::None {
            return Ok(());
        }

        if matches!(event, Event::ProgressUpdate | Event::Buffering) && self.events.contains(&event)
        {
            return Ok(());
        }

        if self.events.len() == self.capacity {
            if let Some(index) = self
                .events
                .iter()
                .position(|queued| *queued == Event::ProgressUpdate)
            {
                self.events.remove(index);
            } else {
                return Err(EventOverflow);
            }
        }

        self.events.push_back(event);
        Ok(())
    }

    pub fn poll(&mut self) -> Event {
        self.events.pop_front().unwrap_or(Event::None)
    }

    pub fn clear(&mut self) {
        self.events.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_keep_order() {
        let mut mailbox = EventMailbox::new(4);
        mailbox.post(Event::StateChanged).unwrap();
        mailbox.post(Event::TrackEnded).unwrap();
        assert_eq!(mailbox.poll(), Event::StateChanged);
        assert_eq!(mailbox.poll(), Event::TrackEnded);
        assert_eq!(mailbox.poll(), Event::None);
    }

    #[test]
    fn progress_events_are_coalesced() {
        let mut mailbox = EventMailbox::new(4);
        mailbox.post(Event::ProgressUpdate).unwrap();
        mailbox.post(Event::ProgressUpdate).unwrap();
        assert_eq!(mailbox.poll(), Event::ProgressUpdate);
        assert_eq!(mailbox.poll(), Event::None);
    }

    #[test]
    fn critical_event_displaces_progress() {
        let mut mailbox = EventMailbox::new(2);
        mailbox.post(Event::ProgressUpdate).unwrap();
        mailbox.post(Event::StateChanged).unwrap();
        mailbox.post(Event::Error).unwrap();
        assert_eq!(mailbox.poll(), Event::StateChanged);
        assert_eq!(mailbox.poll(), Event::Error);
    }

    #[test]
    fn full_critical_mailbox_reports_overflow() {
        let mut mailbox = EventMailbox::new(1);
        mailbox.post(Event::StateChanged).unwrap();
        assert_eq!(mailbox.post(Event::Error), Err(EventOverflow));
    }
}
