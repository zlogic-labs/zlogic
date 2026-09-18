use zlogic_protocol::llm::CacheSpec;

pub const BREAKPOINT_CAP: u32 = 4;

#[derive(Debug)]
pub struct Breakpoints {
    remaining: u32,
    dropped: u32,
    ttl: Option<&'static str>,
}

impl Breakpoints {
    pub fn new(spec: &CacheSpec) -> Self {
        Self {
            remaining: BREAKPOINT_CAP,
            dropped: 0,
            ttl: ttl_bucket(spec.ttl_seconds),
        }
    }

    pub fn take(&mut self) -> bool {
        if self.remaining == 0 {
            self.dropped += 1;
            return false;
        }
        self.remaining -= 1;
        true
    }

    pub fn ttl(&self) -> Option<&'static str> {
        self.ttl
    }

    pub fn dropped(&self) -> u32 {
        self.dropped
    }
}

fn ttl_bucket(seconds: Option<u32>) -> Option<&'static str> {
    seconds.filter(|s| *s >= 3600).map(|_| "1h")
}

#[cfg(test)]
mod tests {
    use super::*;
    use zlogic_protocol::llm::MessageCache;

    #[test]
    fn the_cap_is_enforced_and_the_overflow_is_counted() {
        let mut bp = Breakpoints::new(&CacheSpec::default());
        for i in 0..BREAKPOINT_CAP {
            assert!(bp.take(), "breakpoint {i} should be within the budget");
        }
        assert!(
            !bp.take(),
            "the 5th must be refused — going over the cap is a 400"
        );
        assert!(!bp.take());
        assert_eq!(bp.dropped(), 2, "the dropped count must be reportable");
    }

    #[test]
    fn ttl_has_only_two_buckets() {
        let spec = |ttl: Option<u32>| CacheSpec {
            tools: true,
            system: true,
            messages: MessageCache::LatestUser,
            ttl_seconds: ttl,
            ..Default::default()
        };
        assert_eq!(Breakpoints::new(&spec(None)).ttl(), None);
        assert_eq!(
            Breakpoints::new(&spec(Some(60))).ttl(),
            None,
            "anything under an hour is the default 5m"
        );
        assert_eq!(Breakpoints::new(&spec(Some(3599))).ttl(), None);
        assert_eq!(Breakpoints::new(&spec(Some(3600))).ttl(), Some("1h"));
        assert_eq!(Breakpoints::new(&spec(Some(86_400))).ttl(), Some("1h"));
    }

    #[test]
    fn long_ttl_detection_drives_the_beta_header() {
        assert!(!CacheSpec::default().wants_long_ttl());
        assert!(
            CacheSpec {
                ttl_seconds: Some(3600),
                ..Default::default()
            }
            .wants_long_ttl()
        );
        assert!(
            !CacheSpec {
                ttl_seconds: Some(3600),
                ..CacheSpec::off()
            }
            .wants_long_ttl()
        );
    }
}
