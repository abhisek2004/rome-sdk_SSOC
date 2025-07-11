use {
    solana_sdk::{
        address_lookup_table::AddressLookupTableAccount,
        compute_budget::ComputeBudgetInstruction,
        hash::Hash,
        instruction::Instruction,
        message::{v0, Message, VersionedMessage},
        signature::Keypair,
        signer::{Signer, SignerError},
        transaction::{Transaction, VersionedTransaction},
    },
    std::borrow::Cow,
};

/// An atomic batch of instructions that can be composed into a single transaction
#[derive(Clone)]
pub struct AtomicIxBatch<'a>(Cow<'a, [Instruction]>);

/// [AtomicIxBatch] which owns the instructions with a static lifetime
pub type OwnedAtomicIxBatch = AtomicIxBatch<'static>;

impl AtomicIxBatch<'static> {
    /// # Safety
    ///
    /// Create a new owned [IxBatch] from a list of [Instruction]s
    /// assuming batch size is less than or equal to the maximum transaction size
    pub fn new_owned(ixs: Vec<Instruction>) -> AtomicIxBatch<'static> {
        AtomicIxBatch(Cow::Owned(ixs))
    }

    /// Create multiple composible [IxBatch] from a list of [Instruction]s
    pub fn new_composible_batches_owned(ixs: Vec<Instruction>) -> Vec<AtomicIxBatch<'static>> {
        ixs.into_iter().map(Self::new_composible_owned).collect()
    }
    /// Add system instructions
    pub fn new_composible_owned(ix: Instruction) -> AtomicIxBatch<'static> {
        AtomicIxBatch(Cow::Owned(vec![
            ComputeBudgetInstruction::set_compute_unit_limit(1_400_000),
            ComputeBudgetInstruction::request_heap_frame(256 * 1024),
            ix,
        ]))
    }
    
    /// Add svm instructions
    pub fn push(&mut self, mut ix: Vec<Instruction>) {
        self.0.to_mut().append(&mut ix)
    }
}

impl AtomicIxBatch<'_> {
    /// # Safety
    ///
    /// Unsafely Create a new borrowed [IxBatch] from a list of [Instruction]s
    /// assuming batch size is less than or equal to the maximum transaction size
    pub unsafe fn new_borrowed(ixs: &[Instruction]) -> AtomicIxBatch {
        AtomicIxBatch(Cow::Borrowed(ixs))
    }
    /// Compose a [Transaction] from [IxBatch]
    pub fn compose_legacy_solana_tx(
        &self,
        payer: &Keypair,
        blockhash: Hash,
    ) -> VersionedTransaction {
        let message = Message::new(&self.0, Some(&payer.pubkey()));

        Transaction::new(&[payer], message, blockhash).into()
    }

    pub fn compose_v0_solana_tx(
        &self,
        payer: &Keypair,
        blockhash: Hash,
        alt: &[AddressLookupTableAccount],
    ) -> Result<VersionedTransaction, SignerError> {
        let message = v0::Message::try_compile(&payer.pubkey(), &self.0, alt, blockhash)
            .map_err(|e| SignerError::Custom(e.to_string()))?;

        VersionedTransaction::try_new(VersionedMessage::V0(message), &[payer])
    }

    /// Compose a [Transaction] from [IxBatch] and signers
    pub fn compose_legacy_solana_tx_with_signers(
        &self,
        payer: &Keypair,
        signers: &[&Keypair],
        blockhash: Hash,
    ) -> VersionedTransaction {
        println!("compose_legacy_solana_tx_with_signers");
        let message = Message::new(&self.0, Some(&payer.pubkey()));

        let mut all_signers = vec![payer]; // Start with payer
        all_signers.extend_from_slice(signers); // Add other signers

        all_signers.iter().for_each(|signer| {
            println!("signer pubkey: {:?}", signer.pubkey());
        });
        Transaction::new(&all_signers, message, blockhash).into()
    }
}

impl std::ops::Deref for AtomicIxBatch<'_> {
    type Target = [Instruction];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
