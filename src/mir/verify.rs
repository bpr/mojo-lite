//! Standalone structural verification for typed MIR.

use super::{MirBlock, MirFunction, MirInstr, MirPlace, MirProgram, Proj};

pub fn verify(program: &MirProgram) -> Vec<String> {
    let mut errors = Vec::new();
    for (name, function) in &program.functions {
        verify_function(name, function, &mut errors);
    }
    errors
}

fn verify_function(name: &str, function: &MirFunction, errors: &mut Vec<String>) {
    if function.var_names.len() != function.n_vars {
        errors.push(format!(
            "MIR function '{name}' has {} variable names for {} slots",
            function.var_names.len(),
            function.n_vars
        ));
    }
    verify_blocks(name, function, &function.blocks, errors);
}

fn verify_blocks(
    name: &str,
    function: &MirFunction,
    blocks: &[MirBlock],
    errors: &mut Vec<String>,
) {
    for (block_index, block) in blocks.iter().enumerate() {
        for instruction in &block.instrs {
            for place in instruction_places(instruction) {
                verify_place(name, block_index, function, place, errors);
            }
            if let MirInstr::Try {
                body,
                handler,
                orelse,
                finalbody,
                ..
            } = instruction
            {
                verify_blocks(name, function, body, errors);
                if let Some((_, blocks)) = handler {
                    verify_blocks(name, function, blocks, errors);
                }
                if let Some(blocks) = orelse {
                    verify_blocks(name, function, blocks, errors);
                }
                if let Some(blocks) = finalbody {
                    verify_blocks(name, function, blocks, errors);
                }
            }
        }
    }
}

fn instruction_places(instruction: &MirInstr) -> Vec<&MirPlace> {
    match instruction {
        MirInstr::BeginLoan { place, .. }
        | MirInstr::MakeRef { place, .. }
        | MirInstr::MovePlace { place, .. }
        | MirInstr::Store { place, .. }
        | MirInstr::StoreRef { place, .. }
        | MirInstr::MultiSet {
            receiver_place: place,
            ..
        }
        | MirInstr::LoadPlace { place, .. }
        | MirInstr::VariantSet { place, .. }
        | MirInstr::VariantReplace { place, .. }
        | MirInstr::ConsumePlace { place, .. } => vec![place],
        MirInstr::MakeClosure { captures, .. } => {
            captures.iter().map(|capture| &capture.place).collect()
        }
        MirInstr::Call { arg_places, .. } => arg_places.iter().flatten().collect(),
        MirInstr::MethodCall {
            recv_place,
            arg_places,
            ..
        } => recv_place
            .iter()
            .chain(arg_places.iter().flatten())
            .collect(),
        _ => Vec::new(),
    }
}

fn verify_place(
    function_name: &str,
    block: usize,
    function: &MirFunction,
    place: &MirPlace,
    errors: &mut Vec<String>,
) {
    let prefix = format!("MIR function '{function_name}' block {block}");
    if place.root as usize >= function.n_vars {
        errors.push(format!(
            "{prefix} place has invalid root slot {}",
            place.root
        ));
    }
    if !place.is_typed() {
        errors.push(format!(
            "{prefix} place rooted at slot {} lacks complete checked type metadata",
            place.root
        ));
        return;
    }
    for projection in &place.proj {
        if let Proj::Index(register) = projection
            && register.0 >= function.n_regs
        {
            errors.push(format!(
                "{prefix} place index uses invalid register r{}",
                register.0
            ));
        }
    }
}
