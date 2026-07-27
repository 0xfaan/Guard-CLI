#![no_std]
use soroban_sdk::{contract, contractimpl, symbol_short, Env};

#[contract]
pub struct KeyCollisionSafe;

#[contractimpl]
impl KeyCollisionSafe {
    /// Safe: stores the balance under a distinct key.
    pub fn set_balance(env: Env, balance: i128) {
        env.storage().instance().set(&symbol_short!("bal"), &balance);
    }

    /// Safe: stores the pause flag under a distinct key.
    pub fn set_paused(env: Env, paused: bool) {
        env.storage()
            .instance()
            .set(&symbol_short!("paused"), &paused);
    }
}
