use crate::error::RomeEvmError::TooManyAccounts;
use crate::{
    error::{ProgramResult, RomeEvmError},
    indexer::parsers::log_parser,
};
use emulator::Emulation;
use ethers::types::{NameOrAddress, TransactionRequest, U256};
use rome_evm::{tx::legacy::Legacy as LegacyTx, ExitReason, H160 as EvmH160};

pub struct RomeEvmUtil;

// the feature 9LZdXeKGeBV6hRLdxS1rHbHoEUsKqesCC2ZAPTPKJAbK is not activated on mainnet-beta
const MAX_ALLOWED_ACCOUNTS: usize = 64 - 2; // 2:  address_lookup_table account + program_id(?)

impl RomeEvmUtil {
    /// Convert [U256] to [rome_evm::U256]
    pub fn cast_u256(value: U256) -> rome_evm::U256 {
        let mut buf = [0; 32];

        value.to_big_endian(&mut buf);

        rome_evm::U256::from_big_endian(&buf)
    }

    /// Convert eth [TransactionRequest] to rome [Legacy]
    pub fn cast_transaction_request(value: &TransactionRequest, chain_id: u64) -> LegacyTx {
        LegacyTx {
            nonce: value.nonce.unwrap_or_default().as_u64(), // todo: load from chain?
            gas_price: value.gas_price.map(Self::cast_u256).unwrap_or_default(),
            gas_limit: value.gas.map(Self::cast_u256).unwrap_or_default(),
            to: value.to.clone().map(|v| match v {
                NameOrAddress::Address(addr) => EvmH160::from(addr.0),
                NameOrAddress::Name(_) => EvmH160::default(),
            }),
            value: value.value.map(Self::cast_u256).unwrap_or_default(),
            data: Some(value.data.clone().unwrap_or_default().to_vec()),
            chain_id: value
                .chain_id
                .map(|a| a.as_u64().into())
                .unwrap_or(chain_id.into()),
            from: value.from.map(|v| EvmH160::from(v.0)).unwrap_or_default(),
            ..Default::default()
        }
    }
}

// check for revert
pub fn check_exit_reason(emulation: &Emulation) -> ProgramResult<()> {
    let Some(vm) = emulation.vm.as_ref() else {
        return Ok(());
    };

    match vm.exit_reason {
        ExitReason::Succeed(_) => Ok(()),
        ExitReason::Revert(_) => {
            let mes = vm
                .return_value
                .as_ref()
                .and_then(|value| log_parser::decode_revert(value))
                .map(|a| format!("execution reverted: {}", a))
                .unwrap_or("execution reverted".to_string());

            let data = vm
                .return_value
                .as_ref()
                .map(|a| format!("0x{}", hex::encode(a)))
                .unwrap_or("0x".to_string());

            Err(RomeEvmError::EmulationRevert(mes, data))
        }
        ExitReason::Error(e) => Err(RomeEvmError::EmulationError(format!("{:?}", e))),
        ExitReason::Fatal(e) => Err(RomeEvmError::EmulationError(format!("{:?}", e))),
        ExitReason::StepLimitReached => {
            Err(RomeEvmError::EmulationError("StepLimitReached".to_string()))
        }
    }
}

pub fn check_accounts_len(emulation: &Emulation) -> ProgramResult<()> {
    if emulation.accounts.len() > MAX_ALLOWED_ACCOUNTS {
        return Err(TooManyAccounts(emulation.accounts.len()));
    }

    Ok(())
}
