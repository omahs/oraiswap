use crate::migration::migrate_asset_keys_to_lp_tokens;
use crate::state::{
    read_all_pool_info_keys, read_config, read_is_migrated, read_pool_info, read_rewards_per_sec,
    rewards_read, rewards_store, stakers_read, stakers_store, store_is_migrated, store_pool_info,
    store_rewards_per_sec, PoolInfo, RewardInfo,
};
use cosmwasm_std::{testing::mock_dependencies, Api};
use cosmwasm_std::{Addr, Decimal, Uint128};
use oraiswap::asset::{AssetInfo, AssetInfoRaw, AssetRaw};

const MAINET_STATE_BYTES: &[u8] = include_bytes!("./mainnet.state");

#[test]
fn test_forked_mainnet() {
    let mut deps = mock_dependencies();
    let deps_mut = deps.as_mut();
    let storage = deps_mut.storage;

    // first 4 bytes is for uint32 be
    // 1 byte key length + key
    // 2 bytes value length + value
    let mut ind = 4;

    // let items_length = u32::from_be_bytes(MAINET_STATE_BYTES[0..ind].try_into().unwrap());
    while ind < MAINET_STATE_BYTES.len() {
        let key_length = MAINET_STATE_BYTES[ind];
        ind += 1;
        let key = &MAINET_STATE_BYTES[ind..ind + key_length as usize];
        ind += key_length as usize;
        let value_length = u16::from_be_bytes(MAINET_STATE_BYTES[ind..ind + 2].try_into().unwrap());
        ind += 2;
        let value = &MAINET_STATE_BYTES[ind..ind + value_length as usize];
        ind += value_length as usize;
        storage.set(key, value);
    }

    // milky asset
    let asset_key = deps_mut
        .api
        .addr_canonicalize("orai1gzvndtzceqwfymu2kqhta2jn6gmzxvzqwdgvjw")
        .unwrap();

    // let pool_info = read_pool_info(storage, &asset_key).unwrap();
    let config = read_config(storage).unwrap();

    println!("config {:?}", config);
}

#[test]
fn test_migration() {
    // fixture
    let mut deps = mock_dependencies();
    let first_asset_info = AssetInfo::NativeToken {
        denom: "orai".to_string(),
    };
    let first_old_asset_key = deps
        .api
        .addr_canonicalize(&first_asset_info.to_string())
        .unwrap();
    let second_asset_info = AssetInfo::Token {
        contract_addr: Addr::unchecked("airi"),
    };
    let second_old_asset_key = deps
        .api
        .addr_canonicalize(&second_asset_info.to_string())
        .unwrap();

    let first_staking_token = Addr::unchecked("staking1");
    let second_staking_token = Addr::unchecked("staking2");
    let first_staking_canon = deps
        .api
        .addr_canonicalize(first_staking_token.as_str())
        .unwrap();
    let second_staking_canon = deps
        .api
        .addr_canonicalize(second_staking_token.as_str())
        .unwrap();

    let deps_mut = deps.as_mut();
    let storage = deps_mut.storage;

    // populate fake data, can change to 100 if want
    for n in 0..10u64 {
        let amount = Uint128::from(n);
        let staker = deps_mut
            .api
            .addr_canonicalize(format!("staker{:?}", n.to_string().as_str()).as_str())
            .unwrap();
        let (asset_key, staking_token, is_store_migrated) = if n < 5 {
            (
                first_old_asset_key.clone(),
                first_staking_canon.clone(),
                true,
            )
        } else {
            (
                second_old_asset_key.clone(),
                second_staking_canon.clone(),
                false,
            )
        };
        let pool_info = PoolInfo {
            staking_token: staking_token.clone(),
            pending_reward: amount.clone(),
            total_bond_amount: amount.clone(),
            reward_index: Decimal::zero(),
            migration_params: None,
        };
        store_pool_info(storage, &asset_key, &pool_info).unwrap();
        stakers_store(storage, &asset_key)
            .save(&staker, &true)
            .unwrap();
        if n / 2 == 0 {
            rewards_store(storage, &staker)
                .save(
                    &asset_key,
                    &RewardInfo {
                        native_token: true,
                        index: Decimal::zero(),
                        bond_amount: amount.clone(),
                        pending_reward: amount.clone(),
                        pending_withdraw: vec![],
                    },
                )
                .unwrap();
        }
        if is_store_migrated {
            store_is_migrated(storage, &asset_key, &staker).unwrap();
        }
        if n / 2 != 0 {
            store_rewards_per_sec(
                storage,
                &asset_key,
                vec![AssetRaw {
                    info: AssetInfoRaw::NativeToken {
                        denom: "atom".to_string(),
                    },
                    amount: amount.clone(),
                }],
            )
            .unwrap();
        }
    }

    // check asset keys. They should match with our old asset keys set above
    let pool_info_keys = read_all_pool_info_keys(storage).unwrap();
    assert_eq!(pool_info_keys.len(), 2);
    assert_eq!(pool_info_keys.contains(&first_old_asset_key.to_vec()), true);
    assert_eq!(
        pool_info_keys.contains(&second_old_asset_key.to_vec()),
        true
    );

    // action
    migrate_asset_keys_to_lp_tokens(deps_mut.api, storage).unwrap();

    // assert
    // query to see if the stores have been migrated successfully
    // the keys should be staking1 and staking2
    let pool_info_keys = read_all_pool_info_keys(storage).unwrap();
    // should only have two staking token keys
    assert_eq!(pool_info_keys.len(), 2);
    assert_eq!(
        pool_info_keys.contains(&first_staking_canon.clone().to_vec()),
        true
    );
    assert_eq!(
        pool_info_keys.contains(&second_staking_canon.clone().to_vec()),
        true
    );
    // keys already deleted
    assert_eq!(
        pool_info_keys.contains(&first_old_asset_key.to_vec()),
        false
    );
    assert_eq!(
        pool_info_keys.contains(&second_old_asset_key.to_vec()),
        false
    );

    for n in 0..10u64 {
        let amount = Uint128::from(n);
        let staker = deps_mut
            .api
            .addr_canonicalize(format!("staker{:?}", n.to_string().as_str()).as_str())
            .unwrap();

        let (staking_token, is_store_migrated) = if n < 5 {
            (first_staking_canon.clone(), true)
        } else {
            (second_staking_canon.clone(), false)
        };
        assert_eq!(
            stakers_read(storage, &staking_token).load(&staker).unwrap(),
            true
        );

        assert_eq!(
            read_is_migrated(storage, &staking_token, &staker),
            is_store_migrated
        );

        if n / 2 == 0 {
            assert_eq!(
                rewards_read(storage, &staker)
                    .load(&staking_token)
                    .unwrap()
                    .bond_amount,
                amount.clone()
            );
        }

        if n / 2 != 0 {
            assert_eq!(
                read_rewards_per_sec(storage, &staking_token).unwrap().len(),
                1
            );
        }
    }
}
