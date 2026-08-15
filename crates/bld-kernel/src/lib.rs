#![forbid(unsafe_code)]

use async_trait::async_trait;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BoundaryOutcome<S, E> {
    Undefined,
    Denied(E),
    Committed(S),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution<P, E> {
    Undefined,
    Denied(E),
    Ready(P),
}

impl<P, E> From<Result<P, E>> for Resolution<P, E> {
    fn from(value: Result<P, E>) -> Self {
        match value {
            Ok(plan) => Self::Ready(plan),
            Err(error) => Self::Denied(error),
        }
    }
}

#[async_trait]
pub trait BoundaryDomain: Send + Sync {
    type State: Clone + Send + Sync;
    type Proposal: Send;
    type Authority: Send + Sync;
    type Context: Send;
    type Plan: Send + Sync;
    type Evidence: Send;
    type Error: Send;

    async fn resolve(
        &self,
        state: &Self::State,
        proposal: Self::Proposal,
        authority: &Self::Authority,
        context: &Self::Context,
    ) -> Resolution<Self::Plan, Self::Error>;

    async fn execute(
        &self,
        plan: &Self::Plan,
        context: &mut Self::Context,
    ) -> Result<Self::Evidence, Self::Error>;

    async fn validate(
        &self,
        current: &Self::State,
        plan: &Self::Plan,
        evidence: &Self::Evidence,
        context: &Self::Context,
    ) -> Result<Self::State, Self::Error>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Kernel;

impl Kernel {
    pub async fn apply<D: BoundaryDomain>(
        &self,
        domain: &D,
        state: &mut D::State,
        proposal: D::Proposal,
        authority: &D::Authority,
        context: &mut D::Context,
    ) -> BoundaryOutcome<D::State, D::Error> {
        let plan = match domain
            .resolve(state, proposal, authority, context)
            .await
        {
            Resolution::Undefined => return BoundaryOutcome::Undefined,
            Resolution::Denied(error) => return BoundaryOutcome::Denied(error),
            Resolution::Ready(plan) => plan,
        };

        let evidence = match domain.execute(&plan, context).await {
            Ok(evidence) => evidence,
            Err(error) => return BoundaryOutcome::Denied(error),
        };

        let next = match domain.validate(state, &plan, &evidence, context).await {
            Ok(next) => next,
            Err(error) => return BoundaryOutcome::Denied(error),
        };

        *state = next.clone();
        BoundaryOutcome::Committed(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum State {
        Start,
        Done,
    }

    #[derive(Clone, Copy)]
    enum Proposal {
        Go,
        Impossible,
    }

    #[derive(Clone, Copy)]
    struct Authority {
        allowed: bool,
    }

    #[derive(Default)]
    struct Context {
        executed: usize,
    }

    #[derive(Clone, Copy)]
    struct Plan;

    #[derive(Clone, Copy)]
    struct Evidence;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Error {
        Denied,
        Execute,
        Validate,
    }

    struct Domain {
        fail_execute: bool,
        fail_validate: bool,
    }

    #[async_trait]
    impl BoundaryDomain for Domain {
        type State = State;
        type Proposal = Proposal;
        type Authority = Authority;
        type Context = Context;
        type Plan = Plan;
        type Evidence = Evidence;
        type Error = Error;

        async fn resolve(
            &self,
            state: &Self::State,
            proposal: Self::Proposal,
            authority: &Self::Authority,
            _context: &Self::Context,
        ) -> Resolution<Self::Plan, Self::Error> {
            match (state, proposal) {
                (State::Start, Proposal::Go) if authority.allowed => Resolution::Ready(Plan),
                (State::Start, Proposal::Go) => Resolution::Denied(Error::Denied),
                _ => Resolution::Undefined,
            }
        }

        async fn execute(
            &self,
            _plan: &Self::Plan,
            context: &mut Self::Context,
        ) -> Result<Self::Evidence, Self::Error> {
            context.executed += 1;
            if self.fail_execute {
                Err(Error::Execute)
            } else {
                Ok(Evidence)
            }
        }

        async fn validate(
            &self,
            _current: &Self::State,
            _plan: &Self::Plan,
            _evidence: &Self::Evidence,
            _context: &Self::Context,
        ) -> Result<Self::State, Self::Error> {
            if self.fail_validate {
                Err(Error::Validate)
            } else {
                Ok(State::Done)
            }
        }
    }

    #[tokio::test]
    async fn undefined_does_not_execute_or_commit() {
        let domain = Domain { fail_execute: false, fail_validate: false };
        let mut state = State::Start;
        let mut context = Context::default();

        let outcome = Kernel
            .apply(
                &domain,
                &mut state,
                Proposal::Impossible,
                &Authority { allowed: true },
                &mut context,
            )
            .await;

        assert_eq!(outcome, BoundaryOutcome::Undefined);
        assert_eq!(state, State::Start);
        assert_eq!(context.executed, 0);
    }

    #[tokio::test]
    async fn denied_does_not_execute_or_commit() {
        let domain = Domain { fail_execute: false, fail_validate: false };
        let mut state = State::Start;
        let mut context = Context::default();

        let outcome = Kernel
            .apply(
                &domain,
                &mut state,
                Proposal::Go,
                &Authority { allowed: false },
                &mut context,
            )
            .await;

        assert_eq!(outcome, BoundaryOutcome::Denied(Error::Denied));
        assert_eq!(state, State::Start);
        assert_eq!(context.executed, 0);
    }

    #[tokio::test]
    async fn validation_failure_does_not_commit() {
        let domain = Domain { fail_execute: false, fail_validate: true };
        let mut state = State::Start;
        let mut context = Context::default();

        let outcome = Kernel
            .apply(
                &domain,
                &mut state,
                Proposal::Go,
                &Authority { allowed: true },
                &mut context,
            )
            .await;

        assert_eq!(outcome, BoundaryOutcome::Denied(Error::Validate));
        assert_eq!(state, State::Start);
        assert_eq!(context.executed, 1);
    }

    #[tokio::test]
    async fn valid_turn_commits() {
        let domain = Domain { fail_execute: false, fail_validate: false };
        let mut state = State::Start;
        let mut context = Context::default();

        let outcome = Kernel
            .apply(
                &domain,
                &mut state,
                Proposal::Go,
                &Authority { allowed: true },
                &mut context,
            )
            .await;

        assert_eq!(outcome, BoundaryOutcome::Committed(State::Done));
        assert_eq!(state, State::Done);
        assert_eq!(context.executed, 1);
    }
}
