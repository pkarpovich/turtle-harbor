use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;

pub const EXIT_UNPROCESSABLE: i32 = 65;

#[derive(Debug, Clone, PartialEq)]
pub enum JobOutcome {
    Exited(i32),
    Signaled,
    TimedOut,
    NotStarted,
    ReplyLost,
    PublishFailed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verdict {
    Ack,
    Nak,
    Term,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Delivery {
    pub delivered: u32,
    pub max_deliver: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct JobInput {
    pub payload: String,
    pub subject: String,
    pub delivery: Delivery,
    pub traceparent: Option<String>,
    pub result_path: Option<PathBuf>,
}

pub fn verdict(outcome: JobOutcome, delivery: Delivery) -> Verdict {
    let Delivery {
        delivered,
        max_deliver,
    } = delivery;
    let retry = if delivered < max_deliver {
        Verdict::Nak
    } else {
        Verdict::Term
    };

    match outcome {
        JobOutcome::Exited(code) => {
            if code == 0 {
                Verdict::Ack
            } else if code == EXIT_UNPROCESSABLE {
                Verdict::Term
            } else {
                retry
            }
        }
        JobOutcome::Signaled => retry,
        JobOutcome::TimedOut => retry,
        JobOutcome::PublishFailed => retry,
        JobOutcome::NotStarted => Verdict::Nak,
        JobOutcome::ReplyLost => Verdict::Nak,
    }
}

pub fn job_env(input: &JobInput) -> HashMap<OsString, OsString> {
    let JobInput {
        payload,
        subject,
        delivery,
        traceparent,
        result_path,
    } = input;
    let Delivery {
        delivered,
        max_deliver,
    } = delivery;

    let mut env: HashMap<OsString, OsString> = HashMap::new();
    env.insert("TH_JOB_PAYLOAD".into(), payload.into());
    env.insert("TH_JOB_SUBJECT".into(), subject.into());
    env.insert("TH_JOB_DELIVERED".into(), delivered.to_string().into());
    env.insert("TH_JOB_MAX_DELIVER".into(), max_deliver.to_string().into());

    if let Some(traceparent) = traceparent {
        env.insert("TRACEPARENT".into(), traceparent.into());
    }

    if let Some(result_path) = result_path {
        env.insert("TH_JOB_RESULT".into(), result_path.clone().into_os_string());
    }

    env
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_outcomes() -> Vec<JobOutcome> {
        vec![
            JobOutcome::Exited(0),
            JobOutcome::Exited(1),
            JobOutcome::Exited(EXIT_UNPROCESSABLE),
            JobOutcome::Signaled,
            JobOutcome::TimedOut,
            JobOutcome::NotStarted,
            JobOutcome::ReplyLost,
            JobOutcome::PublishFailed,
        ]
    }

    fn expected_below_limit(outcome: &JobOutcome) -> Verdict {
        match outcome {
            JobOutcome::Exited(0) => Verdict::Ack,
            JobOutcome::Exited(EXIT_UNPROCESSABLE) => Verdict::Term,
            JobOutcome::Exited(code) => {
                assert_eq!(*code, 1);
                Verdict::Nak
            }
            JobOutcome::Signaled => Verdict::Nak,
            JobOutcome::TimedOut => Verdict::Nak,
            JobOutcome::NotStarted => Verdict::Nak,
            JobOutcome::ReplyLost => Verdict::Nak,
            JobOutcome::PublishFailed => Verdict::Nak,
        }
    }

    fn expected_at_limit(outcome: &JobOutcome) -> Verdict {
        match outcome {
            JobOutcome::Exited(0) => Verdict::Ack,
            JobOutcome::Exited(EXIT_UNPROCESSABLE) => Verdict::Term,
            JobOutcome::Exited(code) => {
                assert_eq!(*code, 1);
                Verdict::Term
            }
            JobOutcome::Signaled => Verdict::Term,
            JobOutcome::TimedOut => Verdict::Term,
            JobOutcome::NotStarted => Verdict::Nak,
            JobOutcome::ReplyLost => Verdict::Nak,
            JobOutcome::PublishFailed => Verdict::Term,
        }
    }

    #[test]
    fn test_verdict_below_max_deliver() {
        let delivery = Delivery {
            delivered: 2,
            max_deliver: 5,
        };
        for outcome in all_outcomes() {
            let expected = expected_below_limit(&outcome);
            assert_eq!(
                verdict(outcome.clone(), delivery),
                expected,
                "outcome {outcome:?}"
            );
        }
    }

    #[test]
    fn test_verdict_at_max_deliver() {
        let delivery = Delivery {
            delivered: 5,
            max_deliver: 5,
        };
        for outcome in all_outcomes() {
            let expected = expected_at_limit(&outcome);
            assert_eq!(
                verdict(outcome.clone(), delivery),
                expected,
                "outcome {outcome:?}"
            );
        }
    }

    #[test]
    fn test_verdict_above_max_deliver() {
        let delivery = Delivery {
            delivered: 9,
            max_deliver: 5,
        };
        assert_eq!(verdict(JobOutcome::Exited(1), delivery), Verdict::Term);
        assert_eq!(verdict(JobOutcome::Exited(0), delivery), Verdict::Ack);
        assert_eq!(verdict(JobOutcome::NotStarted, delivery), Verdict::Nak);
        assert_eq!(verdict(JobOutcome::ReplyLost, delivery), Verdict::Nak);
    }

    #[test]
    fn test_job_env_all_variables() {
        let input = JobInput {
            payload: "{\"id\":1}".to_string(),
            subject: "recordings.completed".to_string(),
            delivery: Delivery {
                delivered: 3,
                max_deliver: 7,
            },
            traceparent: Some("00-abc-def-01".to_string()),
            result_path: Some(PathBuf::from("/tmp/turtle-harbor-jobs/x.result.json")),
        };

        let env = job_env(&input);

        assert_eq!(env.len(), 6);
        assert_eq!(
            env.get(&OsString::from("TH_JOB_PAYLOAD")).unwrap(),
            "{\"id\":1}"
        );
        assert_eq!(
            env.get(&OsString::from("TH_JOB_SUBJECT")).unwrap(),
            "recordings.completed"
        );
        assert_eq!(env.get(&OsString::from("TH_JOB_DELIVERED")).unwrap(), "3");
        assert_eq!(env.get(&OsString::from("TH_JOB_MAX_DELIVER")).unwrap(), "7");
        assert_eq!(
            env.get(&OsString::from("TRACEPARENT")).unwrap(),
            "00-abc-def-01"
        );
        assert_eq!(
            env.get(&OsString::from("TH_JOB_RESULT")).unwrap(),
            "/tmp/turtle-harbor-jobs/x.result.json"
        );
    }

    #[test]
    fn test_job_env_optional_variables_absent() {
        let input = JobInput {
            payload: "payload".to_string(),
            subject: "recordings.completed".to_string(),
            delivery: Delivery {
                delivered: 1,
                max_deliver: 5,
            },
            traceparent: None,
            result_path: None,
        };

        let env = job_env(&input);

        assert_eq!(env.len(), 4);
        assert!(!env.contains_key(&OsString::from("TRACEPARENT")));
        assert!(!env.contains_key(&OsString::from("TH_JOB_RESULT")));
    }
}
