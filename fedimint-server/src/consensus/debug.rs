use std::fmt;

use crate::ConsensusItem;

/// A newtype for a nice [`fmt::Debug`] of a [`ConsensusItem`]
pub struct DebugConsensusItem<'ci>(pub &'ci ConsensusItem);

impl<'ci> fmt::Debug for DebugConsensusItem<'ci> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            ConsensusItem::Module(mci) => {
                f.write_fmt(format_args!(
                    "Module CI: module={} ci={}",
                    mci.module_instance_id(),
                    mci
                ))?;
            }
            ConsensusItem::Transaction(tx) => {
                f.write_fmt(format_args!(
                    "Transaction txid={}, inputs_num={}, outputs_num={}",
                    tx.tx_hash(),
                    tx.inputs.len(),
                    tx.outputs.len(),
                ))?;
                // TODO: This is kind of lengthy, and maybe could be conditionally enabled
                // via an env var or something.
                for input in &tx.inputs {
                    // TODO: add pretty print fn to interface
                    f.write_fmt(format_args!("\n    Input: {input}"))?;
                }
                for output in &tx.outputs {
                    f.write_fmt(format_args!("\n    Output: {output}")).unwrap();
                }
            }
            ConsensusItem::Default { variant, .. } => {
                f.write_fmt(format_args!("Unknown CI variant: {variant}"))?;
            }
        }
        Ok(())
    }
}
