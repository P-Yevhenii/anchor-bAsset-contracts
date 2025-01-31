#[cfg(not(feature = "library"))]
use cosmwasm_std::entry_point;
use cosmwasm_std::{attr, from_binary, to_binary, Addr, Binary, CosmosMsg, Decimal, Deps, DepsMut, DistributionMsg, Env, MessageInfo, QueryRequest, Response, StakingMsg, StdError, StdResult, SubMsg, Uint128, WasmMsg, WasmQuery, BankMsg, Coin};

use crate::config::{
    execute_deregister_validator, execute_register_validator, execute_update_config,
    execute_update_params,
};

use crate::state::{
    all_unbond_history, get_unbond_requests, query_get_finished_amount, read_valid_validators,
    CurrentBatch, Parameters, CONFIG, CURRENT_BATCH, PARAMETERS, STATE,
};
use crate::unbond::{execute_unbond, execute_withdraw_unbonded};

use crate::bond::execute_bond;
use basset::hub::ExecuteMsg::SwapHook;
use basset::hub::{
    AllHistoryResponse, Config, ConfigResponse, CurrentBatchResponse, Cw20HookMsg, ExecuteMsg,
    InstantiateMsg, QueryMsg, State, StateResponse, UnbondRequestsResponse,
    WhitelistedValidatorsResponse, WithdrawableUnbondedResponse,
};
use basset::reward::ExecuteMsg::{SwapToRewardDenom, UpdateGlobalIndex};
use cw20::{BalanceResponse, Cw20ExecuteMsg, Cw20QueryMsg, Cw20ReceiveMsg, TokenInfoResponse};
use basset::deduct_tax;

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn instantiate(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    msg: InstantiateMsg,
) -> StdResult<Response> {
    let sender = info.sender.clone();
    let _sndr_raw = deps.api.addr_canonicalize(sender.as_str())?;

    let payment = info
        .funds
        .iter()
        .find(|x| x.denom == msg.underlying_coin_denom && x.amount > Uint128::zero())
        .ok_or_else(|| {
            StdError::generic_err(format!("No {} assets are provided to bond", "uluna"))
        })?;

    // store config
    let data = Config {
        creator: deps.api.addr_canonicalize(info.sender.as_str())?,
        reward_contract: None,
        token_contract: None,
        airdrop_registry_contract: None,
    };
    CONFIG.save(deps.storage, &data)?;

    // store state
    let state = State {
        exchange_rate: Decimal::one(),
        last_index_modification: env.block.time.seconds(),
        last_unbonded_time: env.block.time.seconds(),
        last_processed_batch: 0u64,
        total_bond_amount: payment.amount,
        ..Default::default()
    };

    STATE.save(deps.storage, &state)?;

    // instantiate parameters
    let params = Parameters {
        epoch_period: msg.epoch_period,
        underlying_coin_denom: msg.underlying_coin_denom,
        unbonding_period: msg.unbonding_period,
        peg_recovery_fee: msg.peg_recovery_fee,
        er_threshold: msg.er_threshold,
        reward_denom: msg.reward_denom,
    };

    PARAMETERS.save(deps.storage, &params)?;

    let batch = CurrentBatch {
        id: 1,
        requested_with_fee: Default::default(),
    };
    CURRENT_BATCH.save(deps.storage, &batch)?;

    let mut messages = vec![];

    // register the given validator
    let register_validator = ExecuteMsg::RegisterValidator {
        validator: msg.validator.clone(),
    };
    messages.push(SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: env.contract.address.to_string(),
        msg: to_binary(&register_validator).unwrap(),
        funds: vec![],
    })));

    // send the delegate message
    // messages.push(SubMsg::new(CosmosMsg::Staking(StakingMsg::Delegate {
    //     validator: msg.validator.to_string(),
    //     amount: payment.clone(),
    // })));

    messages.push( SubMsg::new(CosmosMsg::Bank(BankMsg::Send {
        to_address: msg.validator.to_string(),
        amount: vec![deduct_tax(
            &deps.querier,
            Coin {
                denom: payment.clone().denom,
                amount: payment.amount,
            },
        )?],
    })));

    Ok(Response::new()
        .add_submessages(messages)
        .add_attributes(vec![
            attr("register-validator", msg.validator),
            attr("bond", payment.amount),
        ]))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn execute(deps: DepsMut, env: Env, info: MessageInfo, msg: ExecuteMsg) -> StdResult<Response> {
    match msg {
        ExecuteMsg::Receive(msg) => receive_cw20(deps, env, info, msg),
        ExecuteMsg::Bond { validator } => execute_bond(deps, env, info, validator),
        ExecuteMsg::UpdateGlobalIndex { airdrop_hooks } => {
            execute_update_global(deps, env, airdrop_hooks)
        }
        ExecuteMsg::WithdrawUnbonded {} => execute_withdraw_unbonded(deps, env, info),
        ExecuteMsg::RegisterValidator { validator } => {
            execute_register_validator(deps, env, info, validator)
        }
        ExecuteMsg::DeregisterValidator { validator } => {
            execute_deregister_validator(deps, env, info, validator)
        }
        ExecuteMsg::CheckSlashing {} => execute_slashing(deps, env),
        ExecuteMsg::UpdateParams {
            epoch_period,
            unbonding_period,
            peg_recovery_fee,
            er_threshold,
        } => execute_update_params(
            deps,
            env,
            info,
            epoch_period,
            unbonding_period,
            peg_recovery_fee,
            er_threshold,
        ),
        ExecuteMsg::UpdateConfig {
            owner,
            reward_contract,
            token_contract,
            airdrop_registry_contract,
        } => execute_update_config(
            deps,
            env,
            info,
            owner,
            reward_contract,
            token_contract,
            airdrop_registry_contract,
        ),
        ExecuteMsg::SwapHook {
            airdrop_token_contract,
            airdrop_swap_contract,
            swap_msg,
        } => swap_hook(
            deps,
            env,
            info,
            airdrop_token_contract,
            airdrop_swap_contract,
            swap_msg,
        ),
        ExecuteMsg::ClaimAirdrop {
            airdrop_token_contract,
            airdrop_contract,
            airdrop_swap_contract,
            claim_msg,
            swap_msg,
        } => claim_airdrop(
            deps,
            env,
            info,
            airdrop_token_contract,
            airdrop_contract,
            airdrop_swap_contract,
            claim_msg,
            swap_msg,
        ),
    }
}

/// CW20 token receive handler.
pub fn receive_cw20(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    cw20_msg: Cw20ReceiveMsg,
) -> StdResult<Response> {
    let contract_addr = info.sender.clone();

    match from_binary(&cw20_msg.msg) {
        Ok(Cw20HookMsg::Unbond {}) => {
            // only token contract can execute this message
            let conf = CONFIG.load(deps.storage)?;
            if deps.api.addr_canonicalize(contract_addr.as_str())?
                != conf
                    .token_contract
                    .expect("the token contract must have been registered")
            {
                return Err(StdError::generic_err("unauthorized"));
            }
            execute_unbond(deps, env, info, cw20_msg.amount, cw20_msg.sender)
        }
        Err(err) => Err(err),
    }
}

/// Update general parameters
/// Permissionless
pub fn execute_update_global(
    deps: DepsMut,
    env: Env,
    airdrop_hooks: Option<Vec<Binary>>,
) -> StdResult<Response> {
    let mut messages: Vec<SubMsg> = vec![];

    let config = CONFIG.load(deps.storage)?;
    let reward_addr = deps
        .api
        .addr_humanize(
            &config
                .reward_contract
                .expect("the reward contract must have been registered"),
        )?
        .to_string();

    if airdrop_hooks.is_some() {
        let registry_addr = deps
            .api
            .addr_humanize(&config.airdrop_registry_contract.unwrap())?;
        for msg in airdrop_hooks.unwrap() {
            messages.push(SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
                contract_addr: registry_addr.to_string(),
                msg,
                funds: vec![],
            })))
        }
    }

    // Send withdraw message
    let mut withdraw_msgs = withdraw_all_rewards(&deps, env.contract.address.clone())?;
    messages.append(&mut withdraw_msgs);

    // Send Swap message to reward contract
    let swap_msg = SwapToRewardDenom {};
    messages.push(SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: reward_addr.clone(),
        msg: to_binary(&swap_msg).unwrap(),
        funds: vec![],
    })));

    messages.push(SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: reward_addr,
        msg: to_binary(&UpdateGlobalIndex {}).unwrap(),
        funds: vec![],
    })));

    //update state last modified
    STATE.update(deps.storage, |mut last_state| -> StdResult<State> {
        last_state.last_index_modification = env.block.time.seconds();
        Ok(last_state)
    })?;

    Ok(Response::new()
        .add_submessages(messages)
        .add_attributes(vec![attr("action", "update_global_index")]))
}

/// Create withdraw requests for all validators
fn withdraw_all_rewards(deps: &DepsMut, delegator: Addr) -> StdResult<Vec<SubMsg>> {
    let mut messages: Vec<SubMsg> = vec![];
    let delegations = deps.querier.query_all_delegations(delegator);

    if let Ok(delegations) = delegations {
        for delegation in delegations {
            let msg: CosmosMsg =
                CosmosMsg::Distribution(DistributionMsg::WithdrawDelegatorReward {
                    validator: delegation.validator,
                });
            messages.push(SubMsg::new(msg));
        }
    }

    Ok(messages)
}

/// Check whether slashing has happened
/// This is used for checking slashing while bonding or unbonding
pub fn slashing(deps: &mut DepsMut, env: Env) -> StdResult<()> {
    //read params
    let params = PARAMETERS.load(deps.storage)?;
    let coin_denom = params.underlying_coin_denom;

    // Check the amount that contract thinks is bonded
    let state_total_bonded = STATE.load(deps.storage)?.total_bond_amount;

    // Check the actual bonded amount
    let delegations = deps.querier.query_all_delegations(env.contract.address)?;
    if delegations.is_empty() {
        Ok(())
    } else {
        let mut actual_total_bonded = Uint128::zero();
        for delegation in delegations {
            if delegation.amount.denom == coin_denom {
                actual_total_bonded += delegation.amount.amount
            }
        }

        // Need total issued for updating the exchange rate
        let total_issued = query_total_issued(deps.as_ref())?;
        let current_requested_fee = CURRENT_BATCH.load(deps.storage)?.requested_with_fee;

        // Slashing happens if the expected amount is less than stored amount
        if state_total_bonded.u128() > actual_total_bonded.u128() {
            STATE.update(deps.storage, |mut state| -> StdResult<State> {
                state.total_bond_amount = actual_total_bonded;
                state.update_exchange_rate(total_issued, current_requested_fee);
                Ok(state)
            })?;
        }

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
pub fn claim_airdrop(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    airdrop_token_contract: String,
    airdrop_contract: String,
    airdrop_swap_contract: String,
    claim_msg: Binary,
    swap_msg: Binary,
) -> StdResult<Response> {
    let conf = CONFIG.load(deps.storage)?;

    let sender_raw = deps.api.addr_canonicalize(info.sender.as_str())?;

    let airdrop_reg_raw = conf.airdrop_registry_contract.unwrap();
    let airdrop_reg = deps.api.addr_humanize(&airdrop_reg_raw)?;

    if airdrop_reg_raw != sender_raw {
        return Err(StdError::generic_err(format!(
            "Sender must be {}",
            airdrop_reg
        )));
    }

    let mut messages: Vec<SubMsg> = vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: airdrop_contract,
        msg: claim_msg,
        funds: vec![],
    }))];

    messages.push(SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: env.contract.address.to_string(),
        msg: to_binary(&SwapHook {
            airdrop_token_contract,
            airdrop_swap_contract,
            swap_msg,
        })?,
        funds: vec![],
    })));

    Ok(Response::new().add_submessages(messages))
}

pub fn swap_hook(
    deps: DepsMut,
    env: Env,
    info: MessageInfo,
    airdrop_token_contract: String,
    airdrop_swap_contract: String,
    swap_msg: Binary,
) -> StdResult<Response> {
    if info.sender != env.contract.address {
        return Err(StdError::generic_err("unauthorized"));
    }

    let airdrop_token_balance: BalanceResponse =
        deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
            contract_addr: airdrop_token_contract.to_string(),
            msg: to_binary(&Cw20QueryMsg::Balance {
                address: env.contract.address.to_string(),
            })?,
        }))?;

    if airdrop_token_balance.balance == Uint128::new(0) {
        return Err(StdError::generic_err(format!(
            "There is no balance for {} in airdrop token contract {}",
            &env.contract.address, &airdrop_token_contract
        )));
    }
    let messages: Vec<SubMsg> = vec![SubMsg::new(CosmosMsg::Wasm(WasmMsg::Execute {
        contract_addr: airdrop_token_contract.to_string(),
        msg: to_binary(&Cw20ExecuteMsg::Send {
            contract: airdrop_swap_contract,
            amount: airdrop_token_balance.balance,
            msg: swap_msg,
        })?,
        funds: vec![],
    }))];

    Ok(Response::new()
        .add_submessages(messages)
        .add_attributes(vec![
            attr("action", "swap_airdrop_token"),
            attr("token_contract", airdrop_token_contract),
            attr("swap_amount", airdrop_token_balance.balance),
        ]))
}

/// Handler for tracking slashing
pub fn execute_slashing(mut deps: DepsMut, env: Env) -> StdResult<Response> {
    // call slashing
    slashing(&mut deps, env)?;
    // read state for log
    let state = STATE.load(deps.storage)?;
    Ok(Response::new().add_attributes(vec![
        attr("action", "check_slashing"),
        attr("new_exchange_rate", state.exchange_rate.to_string()),
    ]))
}

#[cfg_attr(not(feature = "library"), entry_point)]
pub fn query(deps: Deps, env: Env, msg: QueryMsg) -> StdResult<Binary> {
    match msg {
        QueryMsg::Config {} => to_binary(&query_config(deps)?),
        QueryMsg::State {} => to_binary(&query_state(deps)?),
        QueryMsg::CurrentBatch {} => to_binary(&query_current_batch(deps)?),
        QueryMsg::WhitelistedValidators {} => to_binary(&query_white_validators(deps)?),
        QueryMsg::WithdrawableUnbonded { address } => {
            to_binary(&query_withdrawable_unbonded(deps, address, env)?)
        }
        QueryMsg::Parameters {} => to_binary(&query_params(deps)?),
        QueryMsg::UnbondRequests { address } => to_binary(&query_unbond_requests(deps, address)?),
        QueryMsg::AllHistory { start_from, limit } => {
            to_binary(&query_unbond_requests_limitation(deps, start_from, limit)?)
        }
    }
}

fn query_config(deps: Deps) -> StdResult<ConfigResponse> {
    let config = CONFIG.load(deps.storage)?;
    let mut reward: Option<String> = None;
    let mut token: Option<String> = None;
    let mut airdrop: Option<String> = None;
    if config.reward_contract.is_some() {
        reward = Some(
            deps.api
                .addr_humanize(&config.reward_contract.unwrap())
                .unwrap()
                .to_string(),
        );
    }
    if config.token_contract.is_some() {
        token = Some(
            deps.api
                .addr_humanize(&config.token_contract.unwrap())
                .unwrap()
                .to_string(),
        );
    }
    if config.airdrop_registry_contract.is_some() {
        airdrop = Some(
            deps.api
                .addr_humanize(&config.airdrop_registry_contract.unwrap())
                .unwrap()
                .to_string(),
        );
    }

    Ok(ConfigResponse {
        owner: deps.api.addr_humanize(&config.creator)?.to_string(),
        reward_contract: reward,
        token_contract: token,
        airdrop_registry_contract: airdrop,
    })
}

fn query_state(deps: Deps) -> StdResult<StateResponse> {
    let state = STATE.load(deps.storage)?;
    let res = StateResponse {
        exchange_rate: state.exchange_rate,
        total_bond_amount: state.total_bond_amount,
        last_index_modification: state.last_index_modification,
        prev_hub_balance: state.prev_hub_balance,
        actual_unbonded_amount: state.actual_unbonded_amount,
        last_unbonded_time: state.last_unbonded_time,
        last_processed_batch: state.last_processed_batch,
    };
    Ok(res)
}

fn query_white_validators(deps: Deps) -> StdResult<WhitelistedValidatorsResponse> {
    let validators = read_valid_validators(deps.storage)?;
    let response = WhitelistedValidatorsResponse { validators };
    Ok(response)
}

fn query_current_batch(deps: Deps) -> StdResult<CurrentBatchResponse> {
    let current_batch = CURRENT_BATCH.load(deps.storage)?;
    Ok(CurrentBatchResponse {
        id: current_batch.id,
        requested_with_fee: current_batch.requested_with_fee,
    })
}

fn query_withdrawable_unbonded(
    deps: Deps,
    address: String,
    env: Env,
) -> StdResult<WithdrawableUnbondedResponse> {
    let params = PARAMETERS.load(deps.storage)?;
    let historical_time = env.block.time.seconds() - params.unbonding_period;
    let all_requests = query_get_finished_amount(deps.storage, address, historical_time)?;

    let withdrawable = WithdrawableUnbondedResponse {
        withdrawable: all_requests,
    };
    Ok(withdrawable)
}

fn query_params(deps: Deps) -> StdResult<Parameters> {
    PARAMETERS.load(deps.storage)
}

pub(crate) fn query_total_issued(deps: Deps) -> StdResult<Uint128> {
    let token_address = deps
        .api
        .addr_humanize(
            &CONFIG
                .load(deps.storage)?
                .token_contract
                .expect("token contract must have been registered"),
        )?
        .to_string();
    let token_info: TokenInfoResponse =
        deps.querier.query(&QueryRequest::Wasm(WasmQuery::Smart {
            contract_addr: token_address,
            msg: to_binary(&Cw20QueryMsg::TokenInfo {})?,
        }))?;

    Ok(token_info.total_supply)
}

fn query_unbond_requests(deps: Deps, address: String) -> StdResult<UnbondRequestsResponse> {
    let requests = get_unbond_requests(deps.storage, address.clone())?;
    let res = UnbondRequestsResponse { address, requests };
    Ok(res)
}

fn query_unbond_requests_limitation(
    deps: Deps,
    start: Option<u64>,
    limit: Option<u32>,
) -> StdResult<AllHistoryResponse> {
    let requests = all_unbond_history(deps.storage, start, limit)?;
    let res = AllHistoryResponse { history: requests };
    Ok(res)
}
