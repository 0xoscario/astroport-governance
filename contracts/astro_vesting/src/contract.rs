#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;

use cosmwasm_std::{
    from_binary, to_binary, Addr, Binary, Deps, DepsMut, Empty, Env, MessageInfo, Response,
    StdError, StdResult, Uint128, WasmMsg,
};
use cw2::set_contract_version;
use cw20::{Cw20ExecuteMsg, Cw20ReceiveMsg};

use astroport_governance::astro_vesting::msg::{
    AllocationResponse, ExecuteMsg, InstantiateMsg, QueryMsg, ReceiveMsg, SimulateWithdrawResponse,
    StateResponse,
};
use astroport_governance::astro_vesting::{AllocationParams, AllocationStatus, Config};

use crate::state::{CONFIG, PARAMS, STATE, STATUS};

// version info for migration info
const CONTRACT_NAME: &str = "astro-vesting";
const CONTRACT_VERSION: &str = env!("CARGO_PKG_VERSION");

//----------------------------------------------------------------------------------------
// Entry Points
//----------------------------------------------------------------------------------------

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    set_contract_version(deps.storage, CONTRACT_NAME, CONTRACT_VERSION)?;
    CONFIG.save(
        deps.storage,
        &Config {
            owner: deps.api.addr_validate(&msg.owner)?,
            refund_recepient: deps.api.addr_validate(&msg.refund_recepient)?,
            astro_token: deps.api.addr_validate(&msg.astro_token)?,
        },
    )?;
    Ok(Response::default())
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(deps: DepsMut, env: Env, info: MessageInfo, msg: ExecuteMsg) -> StdResult<Response> {
    match msg {
        ExecuteMsg::Receive(cw20_msg) => execute_receive_cw20(deps, env, info, cw20_msg),
        ExecuteMsg::Withdraw {} => execute_withdraw(deps, env, info),
        ExecuteMsg::TransferOwnership {
            new_owner,
            new_refund_recepient,
        } => execute_transfer_ownership(deps, env, info, new_owner, new_refund_recepient),
        ExecuteMsg::ProposeNewReceiver { new_receiver } => {
            execute_propose_new_receiver(deps, env, info, new_receiver)
        }
        ExecuteMsg::DropNewReceiver {} => execute_drope_new_receiver(deps, env, info),
        ExecuteMsg::ClaimReceiver { prev_receiver } => {
            execute_claim_receiver(deps, env, info, prev_receiver)
        }
    }
}

fn execute_receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> StdResult<Response> {
    match from_binary(&cw20_msg.msg)? {
        ReceiveMsg::CreateAllocations { allocations } => execute_create_allocations(
            deps,
            env,
            info.clone(),
            cw20_msg.sender,
            info.sender,
            cw20_msg.amount,
            allocations,
        ),
    }
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_binary(&query_config(deps, env)?),
        QueryMsg::State {} => to_binary(&query_state(deps)?),
        QueryMsg::Allocation { account } => to_binary(&query_allocation(deps, env, account)?),
        QueryMsg::VestedTokens { account } => to_binary(&query_tokens_vested(deps, env, account)?),
        QueryMsg::SimulateWithdraw { account, timestamp } => {
            to_binary(&query_simulate_withdraw(deps, env, account, timestamp)?)
        }
    }
}

//----------------------------------------------------------------------------------------
// Execute Points
//----------------------------------------------------------------------------------------

/// @dev Admin function facilitating creation of new Allocations
/// @params creator: Function caller address. Needs to be the admin
/// @params deposit_token: Token being deposited, should be ASTRO
/// @params deposit_amount: Number of tokens sent along-with the call, should equal the sum of allocation amount
/// @params allocations: New Allocations being created
fn execute_create_allocations(
    deps: DepsMut,
    _env: Env,
    _info: MessageInfo,
    creator: String,
    deposit_token: Addr,
    deposit_amount: Uint128,
    allocations: Vec<(String, AllocationParams)>,
) -> StdResult<Response> {
    let config = CONFIG.load(deps.storage)?;
    let mut state = STATE.may_load(deps.storage)?.unwrap_or_default();

    if deps.api.addr_validate(&creator)? != config.owner {
        return Err(StdError::generic_err("Only owner can create allocations"));
    }

    if deposit_token != config.astro_token {
        return Err(StdError::generic_err("Only ASTRO token can be deposited"));
    }

    if deposit_amount != allocations.iter().map(|params| params.1.amount).sum() {
        return Err(StdError::generic_err("ASTRO deposit amount mismatch"));
    }

    state.total_astro_deposited += deposit_amount;
    state.remaining_astro_tokens += deposit_amount;

    for allocation in allocations {
        let (user_unchecked, params) = allocation;

        let user = deps.api.addr_validate(&user_unchecked)?;

        match PARAMS.load(deps.storage, &user) {
            Ok(..) => {
                return Err(StdError::generic_err(format!(
                    "Allocation (params) already exists for {}",
                    user
                )));
            }
            Err(..) => {
                PARAMS.save(deps.storage, &user, &params)?;
            }
        }

        match STATUS.load(deps.storage, &user) {
            Ok(..) => {
                return Err(StdError::generic_err(format!(
                    "Allocation (status) already exists for {}",
                    user
                )));
            }
            Err(..) => {
                STATUS.save(deps.storage, &user, &AllocationStatus::new())?;
            }
        }
    }

    STATE.save(deps.storage, &state)?;
    Ok(Response::default())
}

/// @dev Allows allocation receivers to claim their ASTRO tokens that can be withdrawn
fn execute_withdraw(deps: DepsMut, env: Env, info: MessageInfo) -> StdResult<Response> {
    let config = CONFIG.load(deps.storage)?;
    let mut state = STATE.may_load(deps.storage)?.unwrap_or_default();

    let params = PARAMS.load(deps.storage, &info.sender)?;
    let mut status = STATUS.load(deps.storage, &info.sender)?;

    let SimulateWithdrawResponse { astro_to_withdraw } =
        helpers::compute_withdraw_amount(env.block.time.seconds(), &params, &mut status);

    state.remaining_astro_tokens -= astro_to_withdraw;

    // SAVE :: state & allocation
    STATE.save(deps.storage, &state)?;

    // Update status
    STATUS.save(deps.storage, &info.sender, &status)?;

    let mut msgs: Vec<WasmMsg> = vec![];

    if astro_to_withdraw.is_zero() {
        return Err(StdError::generic_err("No vested ASTRO to be withdrawn"));
    }

    msgs.push(WasmMsg::Execute {
        contract_addr: config.astro_token.to_string(),
        msg: to_binary(&Cw20ExecuteMsg::Transfer {
            recipient: info.sender.to_string(),
            amount: astro_to_withdraw,
        })?,
        funds: vec![],
    });

    Ok(Response::new()
        .add_messages(msgs)
        .add_attribute("astro_withdrawn", astro_to_withdraw))
}

/// @dev Admin function to update the owner / refund_recepient addresses
fn execute_transfer_ownership(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    new_owner: Option<String>,
    new_refund_recepient: Option<String>,
) -> StdResult<Response> {
    let mut config = CONFIG.load(deps.storage)?;

    if info.sender != config.owner {
        return Err(StdError::generic_err("Only owner can transfer ownership"));
    }

    if new_owner.is_some() {
        config.owner = deps.api.addr_validate(&new_owner.unwrap())?;
    }

    if new_refund_recepient.is_some() {
        config.refund_recepient = deps.api.addr_validate(&new_refund_recepient.unwrap())?;
    }

    CONFIG.save(deps.storage, &config)?;

    Ok(Response::new())
}

/// @dev Facilitates a user to propose the transfer of the ownership of his allocation to a new terra address.
/// @params new_receiver : Proposed terra address to which the ownership of his allocation is to be transferred
fn execute_propose_new_receiver(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    new_receiver: String,
) -> StdResult<Response> {
    let mut alloc_params = PARAMS.load(deps.storage, &info.sender)?;

    match alloc_params.proposed_receiver {
        Some(proposed_receiver) => {
            return Err(StdError::generic_err(format!(
                "Proposed receiver already set to {}",
                proposed_receiver
            )));
        }
        None => {
            alloc_params.proposed_receiver = Some(deps.api.addr_validate(&new_receiver)?);
            PARAMS.save(deps.storage, &info.sender, &alloc_params)?;
        }
    }

    Ok(Response::new()
        .add_attribute("action", "ProposeNewReceiver")
        .add_attribute("proposed_receiver", new_receiver))
}

/// @dev Facilitates a user to drop the initially proposed receiver for his allocation
fn execute_drope_new_receiver(deps: DepsMut, _env: Env, info: MessageInfo) -> StdResult<Response> {
    let mut alloc_params = PARAMS.load(deps.storage, &info.sender)?;

    match alloc_params.proposed_receiver {
        Some(_) => {
            alloc_params.proposed_receiver = None;
            PARAMS.save(deps.storage, &info.sender, &alloc_params)?;
        }
        None => {
            return Err(StdError::generic_err("Proposed receiver not set"));
        }
    }

    Ok(Response::new()
        .add_attribute("action", "DropNewReceiver")
        .add_attribute(
            "dropped_proposed_receiver",
            alloc_params.proposed_receiver.unwrap(),
        ))
}

/// @dev Allows a proposed receiver of an auction to claim the ownership of that auction
/// @params prev_receiver : User who proposed the info.sender as the proposed terra address to which the ownership of his allocation is to be transferred
fn execute_claim_receiver(
    deps: DepsMut,
    _env: Env,
    info: MessageInfo,
    prev_receiver: String,
) -> StdResult<Response> {
    let mut alloc_params = PARAMS.load(deps.storage, &deps.api.addr_validate(&prev_receiver)?)?;

    match alloc_params.proposed_receiver {
        Some(proposed_receiver) => {
            if proposed_receiver == info.sender {
                // Transfers Allocation Parameters ::
                // 1. Save the allocation against the new receiver
                alloc_params.proposed_receiver = None;
                PARAMS.save(deps.storage, &info.sender, &alloc_params)?;
                // 2. Remove the allocation info of previous owner
                PARAMS.remove(deps.storage, &deps.api.addr_validate(&prev_receiver)?);
                // Transfers Allocation Status ::
                let status = STATUS.load(deps.storage, &deps.api.addr_validate(&prev_receiver)?)?;
                STATUS.save(deps.storage, &info.sender, &status)?;
            } else {
                return Err(StdError::generic_err(format!(
                    "Proposed receiver mismatch, Proposed receiver : {}",
                    proposed_receiver
                )));
            }
        }
        None => {
            return Err(StdError::generic_err("Proposed receiver not set"));
        }
    }

    Ok(Response::new()
        .add_attribute("action", "ClaimReceiver")
        .add_attribute("prev_receiver", prev_receiver)
        .add_attribute("new_receiver", info.sender.to_string()))
}

//----------------------------------------------------------------------------------------
// Query Functions
//----------------------------------------------------------------------------------------

/// @dev Config Query
fn query_config(deps: Deps, _env: Env) -> StdResult<Config> {
    CONFIG.load(deps.storage)
}

/// @dev State Query
pub fn query_state(deps: Deps) -> StdResult<StateResponse> {
    let state = STATE.may_load(deps.storage)?.unwrap_or_default();
    Ok(StateResponse {
        total_astro_deposited: state.total_astro_deposited,
        remaining_astro_tokens: state.remaining_astro_tokens,
    })
}

/// @dev Allocation Query
fn query_allocation(deps: Deps, _env: Env, account: String) -> StdResult<AllocationResponse> {
    let account_checked = deps.api.addr_validate(&account)?;

    Ok(AllocationResponse {
        params: PARAMS.load(deps.storage, &account_checked)?,
        status: STATUS.load(deps.storage, &account_checked)?,
    })
}

fn query_tokens_vested(deps: Deps, env: Env, account: String) -> StdResult<Uint128> {
    let account_checked = deps.api.addr_validate(&account)?;

    let params = PARAMS.load(deps.storage, &account_checked)?;

    Ok(helpers::compute_vested_amount(
        env.block.time.seconds(),
        params.amount,
        &params.vest_schedule,
    ))
}

/// @dev Query function to fetch allocation state at any future timestamp
/// @params account : Account address whose allocation state is to be calculated
/// @params timestamp : Timestamp at which allocation state is to be calculated
fn query_simulate_withdraw(
    deps: Deps,
    env: Env,
    account: String,
    timestamp: Option<u64>,
) -> StdResult<SimulateWithdrawResponse> {
    let account_checked = deps.api.addr_validate(&account)?;

    let params = PARAMS.load(deps.storage, &account_checked)?;
    let mut status = STATUS.load(deps.storage, &account_checked)?;

    let timestamp_ = match timestamp {
        Some(timestamp) => timestamp,
        None => env.block.time.seconds(),
    };

    Ok(helpers::compute_withdraw_amount(
        timestamp_,
        &params,
        &mut status,
    ))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn migrate(_deps: DepsMut, _env: Env, _msg: Empty) -> StdResult<Response> {
    Ok(Response::default())
}

//----------------------------------------------------------------------------------------
// Helper Functions
//----------------------------------------------------------------------------------------

mod helpers {
    use cosmwasm_std::Uint128;

    use astroport_governance::astro_vesting::msg::SimulateWithdrawResponse;
    use astroport_governance::astro_vesting::{AllocationParams, AllocationStatus, Schedule};

    pub fn compute_vested_amount(timestamp: u64, amount: Uint128, schedule: &Schedule) -> Uint128 {
        // Tokens haven't begin vesting
        if timestamp < schedule.start_time {
            Uint128::zero()
        }
        // Tokens vest linearly between start time and end time
        else if timestamp < schedule.start_time + schedule.duration {
            amount.multiply_ratio(timestamp - schedule.start_time, schedule.duration)
        }
        // After end time, all tokens are fully vested/unlocked
        else {
            amount
        }
    }

    pub fn compute_withdraw_amount(
        timestamp: u64,
        params: &AllocationParams,
        status: &mut AllocationStatus,
    ) -> SimulateWithdrawResponse {
        // Before the end of cliff period, no token can be withdrawn
        if timestamp < (params.vest_schedule.start_time + params.vest_schedule.cliff) {
            SimulateWithdrawResponse {
                astro_to_withdraw: Uint128::zero(),
            }
        } else {
            // "Vested" amount
            let astro_vested =
                compute_vested_amount(timestamp, params.amount, &params.vest_schedule);

            // Withdrawable amount is unlocked amount minus the amount already withdrawn
            let astro_withdrawable = astro_vested - status.astro_withdrawn;
            status.astro_withdrawn += astro_withdrawable;

            SimulateWithdrawResponse {
                astro_to_withdraw: astro_withdrawable,
            }
        }
    }
}
