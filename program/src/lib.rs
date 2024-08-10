mod initialize;
mod kick;
mod claim;
mod stake;
mod submit;
mod open;
mod certify; 
mod commit;

use initialize::*;
use submit::*;
use certify;::* 
use open::*;
use claim::*;
use kick::*;
use stake::*;
use commit::*;

use ore_pool_api::instruction::PoolInstruction;
use solana_program::{
    self, account_info::AccountInfo, entrypoint::ProgramResult, program_error::ProgramError,
    pubkey::Pubkey,
};

#[cfg(not(feature = "no-entrypoint"))]
solana_program::entrypoint!(process_instruction);

pub fn process_instruction(
    program_id: &Pubkey,
    accounts: &[AccountInfo],
    data: &[u8],
) -> ProgramResult {
    if program_id.ne(&ore_pool_api::id()) {
        return Err(ProgramError::IncorrectProgramId);
    }

    let (tag, data) = data
        .split_first()
        .ok_or(ProgramError::InvalidInstructionData)?;

    match PoolInstruction::try_from(*tag).or(Err(ProgramError::InvalidInstructionData))? {
        // User
        PoolInstruction::Open => process_open(accounts, data)?,
        PoolInstruction::Claim => process_claim(accounts, data)?,
        PoolInstruction::Stake => process_stake(accounts, data)?,
        
        // Admin
        PoolInstruction::Certify => process_certify(accounts, data)?,
        PoolInstruction::Kick => process_kick(accounts, data)?,
        PoolInstruction::Commit => process_commit(accounts, data)?,
        PoolInstruction::Initialize => process_initialize(accounts, data)?,
        PoolInstruction::Submit => process_submit(accounts, data)?,
    }

    Ok(())
}
