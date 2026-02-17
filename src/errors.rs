use thiserror::Error;

#[derive(Error, Clone, Copy, Debug)]
pub enum VoltrError {
    #[error("Invalid Source Mint")]
    InvalidSourceMint = 0,

    #[error("Math Overflow")]
    MathOverflow = 2,

    #[error("Division By Zero")]
    DivisionByZero = 3,

    #[error("Invalid Amount")]
    InvalidAmount = 4,

    #[error("Withdrawal Waiting Period Not Zero")]
    WithdrawalWaitingPeriodNotZero = 5,

    #[error("Insufficient Idle Balance")]
    InsufficientIdleBalance = 6,
}
