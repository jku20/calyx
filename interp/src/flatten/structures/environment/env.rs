use itertools::Itertools;

use super::{
    assignments::{GroupInterfacePorts, ScheduledAssignments},
    program_counter::ProgramCounter,
};

use super::super::{
    context::Context, index_trait::IndexRange, indexed_map::IndexedMap,
};
use crate::{
    errors::{InterpreterError, InterpreterResult},
    flatten::{
        flat_ir::{
            cell_prototype::{CellPrototype, PrimType1},
            prelude::{
                AssignedValue, AssignmentIdx, BaseIndices,
                CellDefinitionRef::{Local, Ref},
                CellRef, ComponentIdx, ControlNode, GlobalCellIdx,
                GlobalCellRef, GlobalPortIdx, GlobalPortRef, GlobalRefCellIdx,
                GlobalRefPortIdx, GuardIdx, Invoke, PortRef, PortValue,
            },
            wires::guards::Guard,
        },
        primitives::{self, prim_trait::UpdateStatus, Primitive},
        structures::{
            environment::program_counter::ControlPoint, index_trait::IndexRef,
        },
    },
    serialization::data_dump::{DataDump, Dimensions},
    values::Value,
};
use std::fmt::Debug;

pub type PortMap = IndexedMap<GlobalPortIdx, PortValue>;

impl PortMap {
    /// Essentially asserts that the port given is undefined, it errors out if
    /// the port is defined and otherwise does nothing
    pub fn write_undef(
        &mut self,
        target: GlobalPortIdx,
    ) -> InterpreterResult<()> {
        if self[target].is_def() {
            todo!("raise error")
        } else {
            Ok(())
        }
    }

    /// Sets the given index to the given value without checking whether or not
    /// the assignment would conflict with an existing assignment. Should only
    /// be used by cells to set values that may be undefined
    pub fn write_exact_unchecked(
        &mut self,
        target: GlobalPortIdx,
        val: PortValue,
    ) -> UpdateStatus {
        if self[target].is_undef() && val.is_undef()
            || self[target].as_option() == val.as_option()
        {
            UpdateStatus::Unchanged
        } else {
            self[target] = val;
            UpdateStatus::Changed
        }
    }

    /// Sets the given index to undefined without checking whether or not it was
    /// already defined
    #[inline]
    pub fn write_undef_unchecked(&mut self, target: GlobalPortIdx) {
        self[target] = PortValue::new_undef();
    }

    pub fn insert_val(
        &mut self,
        target: GlobalPortIdx,
        val: AssignedValue,
    ) -> InterpreterResult<UpdateStatus> {
        match self[target].as_option() {
            // unchanged
            Some(t) if *t == val => Ok(UpdateStatus::Unchanged),
            // conflict
            // TODO: Fix to make the error more helpful
            Some(t) if t.has_conflict_with(&val) => InterpreterResult::Err(
                InterpreterError::FlatConflictingAssignments {
                    a1: t.clone(),
                    a2: val,
                }
                .into(),
            ),
            // changed
            Some(_) | None => {
                self[target] = PortValue::new(val);
                Ok(UpdateStatus::Changed)
            }
        }
    }
}

pub(crate) type CellMap = IndexedMap<GlobalCellIdx, CellLedger>;
pub(crate) type RefCellMap =
    IndexedMap<GlobalRefCellIdx, Option<GlobalCellIdx>>;
pub(crate) type RefPortMap =
    IndexedMap<GlobalRefPortIdx, Option<GlobalPortIdx>>;
pub(crate) type AssignmentRange = IndexRange<AssignmentIdx>;

pub(crate) struct ComponentLedger {
    pub(crate) index_bases: BaseIndices,
    pub(crate) comp_id: ComponentIdx,
}

impl ComponentLedger {
    /// Convert a relative offset to a global one. Perhaps should take an owned
    /// value rather than a pointer
    pub fn convert_to_global_port(&self, port: &PortRef) -> GlobalPortRef {
        match port {
            PortRef::Local(l) => (&self.index_bases + l).into(),
            PortRef::Ref(r) => (&self.index_bases + r).into(),
        }
    }

    pub fn convert_to_global_cell(&self, cell: &CellRef) -> GlobalCellRef {
        match cell {
            CellRef::Local(l) => (&self.index_bases + l).into(),
            CellRef::Ref(r) => (&self.index_bases + r).into(),
        }
    }
}

/// An enum encapsulating cell functionality. It is either a pointer to a
/// primitive or information about a calyx component instance
pub(crate) enum CellLedger {
    Primitive {
        // wish there was a better option with this one
        cell_dyn: Box<dyn Primitive>,
    },
    Component(ComponentLedger),
}

impl CellLedger {
    fn new_comp(idx: ComponentIdx, env: &Environment) -> Self {
        Self::Component(ComponentLedger {
            index_bases: BaseIndices::new(
                env.ports.peek_next_idx(),
                (env.cells.peek_next_idx().index() + 1).into(),
                env.ref_cells.peek_next_idx(),
                env.ref_ports.peek_next_idx(),
            ),
            comp_id: idx,
        })
    }

    pub fn as_comp(&self) -> Option<&ComponentLedger> {
        match self {
            Self::Component(comp) => Some(comp),
            _ => None,
        }
    }

    #[inline]
    pub fn unwrap_comp(&self) -> &ComponentLedger {
        self.as_comp()
            .expect("Unwrapped cell ledger as component but received primitive")
    }

    #[must_use]
    pub fn as_primitive(&self) -> Option<&dyn Primitive> {
        match self {
            Self::Primitive { cell_dyn } => Some(&**cell_dyn),
            _ => None,
        }
    }

    pub fn unwrap_primitive(&self) -> &dyn Primitive {
        self.as_primitive()
            .expect("Unwrapped cell ledger as primitive but received component")
    }
}

impl Debug for CellLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Primitive { .. } => f.debug_struct("Primitive").finish(),
            Self::Component(ComponentLedger {
                index_bases,
                comp_id,
            }) => f
                .debug_struct("Component")
                .field("index_bases", index_bases)
                .field("comp_id", comp_id)
                .finish(),
        }
    }
}

#[derive(Debug)]
pub struct Environment<'a> {
    /// A map from global port IDs to their current values.
    pub(crate) ports: PortMap,
    /// A map from global cell IDs to their current state and execution info.
    cells: CellMap,
    /// A map from global ref cell IDs to the cell they reference, if any.
    ref_cells: RefCellMap,
    /// A map from global ref port IDs to the port they reference, if any.
    ref_ports: RefPortMap,

    /// The program counter for the whole program execution.
    pc: ProgramCounter,

    /// The immutable context. This is retained for ease of use.
    ctx: &'a Context,
}

impl<'a> Environment<'a> {
    pub fn new(ctx: &'a Context, data_map: Option<DataDump>) -> Self {
        let root = ctx.entry_point;
        let aux = &ctx.secondary[root];

        let mut env = Self {
            ports: PortMap::with_capacity(aux.port_offset_map.count()),
            cells: CellMap::with_capacity(aux.cell_offset_map.count()),
            ref_cells: RefCellMap::with_capacity(
                aux.ref_cell_offset_map.count(),
            ),
            ref_ports: RefPortMap::with_capacity(
                aux.ref_port_offset_map.count(),
            ),
            pc: ProgramCounter::new_empty(),
            ctx,
        };

        let root_node = CellLedger::new_comp(root, &env);
        let root = env.cells.push(root_node);
        env.layout_component(root, data_map);

        // Initialize program counter
        // TODO griffin: Maybe refactor into a separate function
        for (idx, ledger) in env.cells.iter() {
            if let CellLedger::Component(comp) = ledger {
                if let Some(ctrl) = &env.ctx.primary[comp.comp_id].control {
                    env.pc.vec_mut().push(ControlPoint {
                        comp: idx,
                        control_node_idx: *ctrl,
                    })
                }
            }
        }

        env
    }

    /// Internal function used to layout a given component from a cell id
    ///
    /// Layout is handled in the following order:
    /// 1. component signature (input/output)
    /// 2. group hole ports
    /// 3. cells + ports, primitive
    /// 4. sub-components
    /// 5. ref-cells & ports
    fn layout_component(
        &mut self,
        comp: GlobalCellIdx,
        data_map: Option<DataDump>,
    ) {
        let ComponentLedger {
            index_bases,
            comp_id,
        } = self.cells[comp]
            .as_comp()
            .expect("Called layout component with a non-component cell.");
        let comp_aux = &self.ctx.secondary[*comp_id];

        // Insert the component's continuous assignments into the program counter, if non-empty
        let cont_assigns = self.ctx.primary[*comp_id].continuous_assignments;
        if !cont_assigns.is_empty() {
            self.pc.push_continuous_assigns(comp, cont_assigns);
        }

        // first layout the signature
        for sig_port in comp_aux.signature.iter() {
            let idx = self.ports.push(PortValue::new_undef());
            debug_assert_eq!(index_bases + sig_port, idx);
        }
        // second group ports
        for group_idx in comp_aux.definitions.groups() {
            //go
            let go = self.ports.push(PortValue::new_undef());

            //done
            let done = self.ports.push(PortValue::new_undef());

            // quick sanity check asserts
            let go_actual = index_bases + self.ctx.primary[group_idx].go;
            let done_actual = index_bases + self.ctx.primary[group_idx].done;
            // Case 1 - Go defined before done
            if self.ctx.primary[group_idx].go < self.ctx.primary[group_idx].done
            {
                debug_assert_eq!(done, done_actual);
                debug_assert_eq!(go, go_actual);
            }
            // Case 2 - Done defined before go
            else {
                // in this case go is defined after done, so our variable names
                // are backward, but this is not a problem since they are
                // initialized to the same value
                debug_assert_eq!(go, done_actual);
                debug_assert_eq!(done, go_actual);
            }
        }

        for (cell_off, def_idx) in comp_aux.cell_offset_map.iter() {
            let info = &self.ctx.secondary[*def_idx];
            if !info.prototype.is_component() {
                let port_base = self.ports.peek_next_idx();
                for port in info.ports.iter() {
                    let idx = self.ports.push(PortValue::new_undef());
                    debug_assert_eq!(
                        &self.cells[comp].as_comp().unwrap().index_bases + port,
                        idx
                    );
                }
                let cell_dyn = primitives::build_primitive(
                    info, port_base, self.ctx, &data_map,
                );
                let cell = self.cells.push(CellLedger::Primitive { cell_dyn });

                debug_assert_eq!(
                    &self.cells[comp].as_comp().unwrap().index_bases + cell_off,
                    cell
                );
            } else {
                let child_comp = info.prototype.as_component().unwrap();
                let child_comp = CellLedger::new_comp(*child_comp, self);

                let cell = self.cells.push(child_comp);
                debug_assert_eq!(
                    &self.cells[comp].as_comp().unwrap().index_bases + cell_off,
                    cell
                );

                // layout sub-component but don't include the data map
                self.layout_component(cell, None);
            }
        }

        // ref cells and ports are initialized to None
        for (ref_cell, def_idx) in comp_aux.ref_cell_offset_map.iter() {
            let info = &self.ctx.secondary[*def_idx];
            for port_idx in info.ports.iter() {
                let port_actual = self.ref_ports.push(None);
                debug_assert_eq!(
                    &self.cells[comp].as_comp().unwrap().index_bases + port_idx,
                    port_actual
                )
            }
            let cell_actual = self.ref_cells.push(None);
            debug_assert_eq!(
                &self.cells[comp].as_comp().unwrap().index_bases + ref_cell,
                cell_actual
            )
        }
    }

    pub fn get_comp_go(&self, comp: GlobalCellIdx) -> GlobalPortIdx {
        let ledger = self.cells[comp]
            .as_comp()
            .expect("Called get_comp_go with a non-component cell.");

        &ledger.index_bases + self.ctx.primary[ledger.comp_id].go
    }

    pub fn get_comp_done(&self, comp: GlobalCellIdx) -> GlobalPortIdx {
        let ledger = self.cells[comp]
            .as_comp()
            .expect("Called get_comp_done with a non-component cell.");

        &ledger.index_bases + self.ctx.primary[ledger.comp_id].done
    }

    pub fn get_root_done(&self) -> GlobalPortIdx {
        self.get_comp_done(GlobalCellIdx::new(0))
    }
}

// ===================== Environment print implementations =====================
impl<'a> Environment<'a> {
    pub fn _print_env(&self) {
        let root_idx = GlobalCellIdx::new(0);
        let mut hierarchy = Vec::new();
        self._print_component(root_idx, &mut hierarchy)
    }

    fn _print_component(
        &self,
        target: GlobalCellIdx,
        hierarchy: &mut Vec<GlobalCellIdx>,
    ) {
        let info = self.cells[target].as_comp().unwrap();
        let comp = &self.ctx.secondary[info.comp_id];
        hierarchy.push(target);

        // This funky iterator chain first pulls the first element (the
        // entrypoint) and extracts its name. Subsequent element are pairs of
        // global offsets produced by a staggered iteration, yielding `(root,
        // child)` then `(child, grandchild)` and so on. All the strings are
        // finally collected and concatenated with a `.` separator to produce
        // the fully qualified name prefix for the given component instance.
        let name_prefix = hierarchy
            .first()
            .iter()
            .map(|x| {
                let info = self.cells[**x].as_comp().unwrap();
                let prior_comp = &self.ctx.secondary[info.comp_id];
                &self.ctx.secondary[prior_comp.name]
            })
            .chain(hierarchy.iter().zip(hierarchy.iter().skip(1)).map(
                |(l, r)| {
                    let info = self.cells[*l].as_comp().unwrap();
                    let prior_comp = &self.ctx.secondary[info.comp_id];
                    let local_target = r - (&info.index_bases);

                    let def_idx = &prior_comp.cell_offset_map[local_target];

                    let id = &self.ctx.secondary[*def_idx];
                    &self.ctx.secondary[id.name]
                },
            ))
            .join(".");

        for (cell_off, def_idx) in comp.cell_offset_map.iter() {
            let definition = &self.ctx.secondary[*def_idx];

            println!("{}.{}", name_prefix, self.ctx.secondary[definition.name]);
            for port in definition.ports.iter() {
                let definition =
                    &self.ctx.secondary[comp.port_offset_map[port]];
                println!(
                    "    {}: {} ({:?})",
                    self.ctx.secondary[definition.name],
                    self.ports[&info.index_bases + port],
                    &info.index_bases + port
                );
            }

            let cell_idx = &info.index_bases + cell_off;

            if definition.prototype.is_component() {
                self._print_component(cell_idx, hierarchy);
            } else if self.cells[cell_idx]
                .as_primitive()
                .unwrap()
                .has_serializable_state()
            {
                println!(
                    "    INTERNAL_DATA: {}",
                    serde_json::to_string_pretty(
                        &self.cells[cell_idx]
                            .as_primitive()
                            .unwrap()
                            .serialize(None)
                    )
                    .unwrap()
                )
            }
        }

        hierarchy.pop();
    }

    pub fn _print_env_stats(&self) {
        println!("Environment Stats:");
        println!("  Ports: {}", self.ports.len());
        println!("  Cells: {}", self.cells.len());
        println!("  Ref Cells: {}", self.ref_cells.len());
        println!("  Ref Ports: {}", self.ref_ports.len());
    }

    pub fn _print_pc(&self) {
        println!("{:?}", self.pc)
    }
}

/// A wrapper struct for the environment that provides the functions used to
/// simulate the actual program. This is just to keep the simulation logic under
/// a different namespace than the environment to avoid confusion
pub struct Simulator<'a> {
    env: Environment<'a>,
}

impl<'a> Simulator<'a> {
    pub fn new(env: Environment<'a>) -> Self {
        let mut output = Self { env };
        output.set_root_go_high();
        output
    }

    pub fn _print_env(&self) {
        self.env._print_env()
    }

    #[inline]
    pub fn ctx(&self) -> &Context {
        self.env.ctx
    }

    pub fn _unpack_env(self) -> Environment<'a> {
        self.env
    }
}

// =========================== simulation functions ===========================
impl<'a> Simulator<'a> {
    #[inline]
    fn lookup_global_port_id(&self, port: GlobalPortRef) -> GlobalPortIdx {
        match port {
            GlobalPortRef::Port(p) => p,
            // TODO Griffin: Please make sure this error message is correct with
            // respect to the compiler
            GlobalPortRef::Ref(r) => self.env.ref_ports[r].expect("A ref port is being queried without a supplied ref-cell. This is an error?"),
        }
    }

    #[inline]
    fn lookup_global_cell_id(&self, cell: GlobalCellRef) -> GlobalCellIdx {
        match cell {
            GlobalCellRef::Cell(c) => c,
            // TODO Griffin: Please make sure this error message is correct with
            // respect to the compiler
            GlobalCellRef::Ref(r) => self.env.ref_cells[r].expect("A ref cell is being queried without a supplied ref-cell. This is an error?"),
        }
    }

    #[inline]
    fn get_global_port_idx(
        &self,
        port: &PortRef,
        comp: GlobalCellIdx,
    ) -> GlobalPortIdx {
        let ledger = self.env.cells[comp].unwrap_comp();
        self.lookup_global_port_id(ledger.convert_to_global_port(port))
    }

    #[inline]
    fn get_global_cell_idx(
        &self,
        cell: &CellRef,
        comp: GlobalCellIdx,
    ) -> GlobalCellIdx {
        let ledger = self.env.cells[comp].unwrap_comp();
        self.lookup_global_cell_id(ledger.convert_to_global_cell(cell))
    }

    #[inline]
    fn get_value(&self, port: &PortRef, comp: GlobalCellIdx) -> &PortValue {
        let port_idx = self.get_global_port_idx(port, comp);
        &self.env.ports[port_idx]
    }

    pub(crate) fn get_root_component(&self) -> &ComponentLedger {
        self.env.cells.first().unwrap().as_comp().unwrap()
    }

    /// Finds the root component of the simulation and sets its go port to high
    fn set_root_go_high(&mut self) {
        let ledger = self.get_root_component();
        let go = &ledger.index_bases + self.env.ctx.primary[ledger.comp_id].go;
        self.env.ports[go] = PortValue::new_implicit(Value::bit_high());
    }

    /// Attempt to find the parent cell for a port. If no such cell exists (i.e.
    /// it is a hole port, then it returns None)
    fn _get_parent_cell(
        &self,
        port: PortRef,
        comp: GlobalCellIdx,
    ) -> Option<GlobalCellIdx> {
        let component = self.env.cells[comp].unwrap_comp();
        let comp_info = &self.env.ctx.secondary[component.comp_id];

        match port {
            PortRef::Local(l) => {
                for (cell_offset, cell_def_idx) in
                    comp_info.cell_offset_map.iter()
                {
                    if self.env.ctx.secondary[*cell_def_idx].ports.contains(l) {
                        return Some(&component.index_bases + cell_offset);
                    }
                }
            }
            PortRef::Ref(r) => {
                for (cell_offset, cell_def_idx) in
                    comp_info.ref_cell_offset_map.iter()
                {
                    if self.env.ctx.secondary[*cell_def_idx].ports.contains(r) {
                        let ref_cell_idx = &component.index_bases + cell_offset;
                        return Some(
                            self.env.ref_cells[ref_cell_idx]
                                .expect("Ref cell has not been instantiated"),
                        );
                    }
                }
            }
        }

        None
    }

    // may want to make this iterate directly if it turns out that the vec
    // allocation is too expensive in this context
    fn get_assignments(
        &self,
        control_points: &[ControlPoint],
    ) -> Vec<ScheduledAssignments> {
        control_points
            .iter()
            .map(|node| {
                match &self.ctx().primary[node.control_node_idx] {
                    ControlNode::Enable(e) => {
                        let group = &self.ctx().primary[e.group()];

                        ScheduledAssignments::new(
                            node.comp,
                            group.assignments,
                            Some(GroupInterfacePorts {
                                go: group.go,
                                done: group.done,
                            }),
                        )
                    }

                    ControlNode::Invoke(i) => ScheduledAssignments::new(
                        node.comp,
                        i.assignments,
                        None,
                    ),

                    ControlNode::Empty(_) => {
                        unreachable!(
                            "called `get_assignments` with an empty node"
                        )
                    }
                    // non-leaf nodes
                    ControlNode::If(_)
                    | ControlNode::While(_)
                    | ControlNode::Seq(_)
                    | ControlNode::Par(_) => {
                        unreachable!(
                            "Called `get_assignments` with non-leaf nodes"
                        )
                    }
                }
            })
            .chain(
                self.env.pc.continuous_assigns().iter().map(|x| {
                    ScheduledAssignments::new(x.comp, x.assigns, None)
                }),
            )
            .chain(self.env.pc.with_map().iter().map(|(ctrl_pt, comb_grp)| {
                let assigns = self.ctx().primary[*comb_grp].assignments;
                ScheduledAssignments::new(ctrl_pt.comp, assigns, None)
            }))
            .collect()
    }

    /// A helper function which inserts indicies for the ref cells and ports
    /// used in the invoke statement
    fn intialize_ref_cells(
        &mut self,
        parent_comp: GlobalCellIdx,
        invoke: &Invoke,
    ) {
        if invoke.ref_cells.is_empty() {
            return;
        }

        let parent_ledger = self.env.cells[parent_comp].unwrap_comp();
        let parent_info = &self.env.ctx.secondary[parent_ledger.comp_id];

        let child_comp = self.get_global_cell_idx(&invoke.cell, parent_comp);
        // this unwrap should never fail because ref-cells can only exist on
        // components, not primitives
        let child_ledger = self.env.cells[child_comp]
            .as_comp()
            .expect("malformed invoke?");
        let child_info = &self.env.ctx.secondary[child_ledger.comp_id];

        for (offset, cell_ref) in invoke.ref_cells.iter() {
            // first set the ref cell
            let global_ref_cell_idx = &child_ledger.index_bases + offset;
            let global_actual_cell_idx =
                self.get_global_cell_idx(cell_ref, parent_comp);
            self.env.ref_cells[global_ref_cell_idx] =
                Some(global_actual_cell_idx);

            // then set the ports
            let child_ref_cell_info = &self.env.ctx.secondary
                [child_info.ref_cell_offset_map[*offset]];

            let cell_info_idx = parent_info.get_cell_info_idx(*cell_ref);
            match cell_info_idx {
                Local(l) => {
                    let info = &self.env.ctx.secondary[l];
                    assert_eq!(
                        child_ref_cell_info.ports.size(),
                        info.ports.size()
                    );

                    for (dest, source) in
                        child_ref_cell_info.ports.iter().zip(info.ports.iter())
                    {
                        let dest_idx = &child_ledger.index_bases + dest;
                        let source_idx = &parent_ledger.index_bases + source;
                        self.env.ref_ports[dest_idx] = Some(source_idx);
                    }
                }
                Ref(r) => {
                    let info = &self.env.ctx.secondary[r];
                    assert_eq!(
                        child_ref_cell_info.ports.size(),
                        info.ports.size()
                    );

                    for (dest, source) in
                        child_ref_cell_info.ports.iter().zip(info.ports.iter())
                    {
                        let dest_idx = &child_ledger.index_bases + dest;
                        let source_ref_idx =
                            &parent_ledger.index_bases + source;
                        // TODO griffin: Make this error message actually useful
                        let source_idx_actual = self.env.ref_ports
                            [source_ref_idx]
                            .expect("ref port not instantiated, this is a bug");

                        self.env.ref_ports[dest_idx] = Some(source_idx_actual);
                    }
                }
            }
        }
    }

    fn cleanup_ref_cells(
        &mut self,
        parent_comp: GlobalCellIdx,
        invoke: &Invoke,
    ) {
        let child_comp = self.get_global_cell_idx(&invoke.cell, parent_comp);
        // this unwrap should never fail because ref-cells can only exist on
        // components, not primitives
        let child_ledger = self.env.cells[child_comp]
            .as_comp()
            .expect("malformed invoke?");
        let child_info = &self.env.ctx.secondary[child_ledger.comp_id];

        for (offset, _) in invoke.ref_cells.iter() {
            // first unset the ref cell
            let global_ref_cell_idx = &child_ledger.index_bases + offset;
            self.env.ref_cells[global_ref_cell_idx] = None;

            // then unset the ports
            let child_ref_cell_info = &self.env.ctx.secondary
                [child_info.ref_cell_offset_map[*offset]];

            for port in child_ref_cell_info.ports.iter() {
                let port_idx = &child_ledger.index_bases + port;
                self.env.ref_ports[port_idx] = None;
            }
        }
    }

    pub fn step(&mut self) -> InterpreterResult<()> {
        // place to keep track of what groups we need to conclude at the end of
        // this step. These are indices into the program counter

        // In the future it may be worthwhile to preallocate some space to these
        // buffers. Can pick anything from zero to the number of nodes in the
        // program counter as the size
        let mut leaf_nodes = vec![];
        let mut set_done = vec![];

        let mut new_nodes = vec![];
        let (mut vecs, mut par_map, mut with_map) = self.env.pc.take_fields();

        // TODO griffin: This has become an unwieldy mess and should really be
        // refactored into a handful of internal functions
        vecs.retain_mut(|node| {
            let comp_go = self.env.get_comp_go(node.comp);
            let comp_done = self.env.get_comp_done(node.comp);
            if !self.env.ports[comp_go].as_bool().unwrap_or_default() || self.env.ports[comp_done].as_bool().unwrap_or_default() {
                // if the go port is low or the done port is high, we skip the node without doing anything
                return true;
            }

            // just considering a single node case for the moment
            let retain_bool = match &self.env.ctx.primary[node.control_node_idx] {
                ControlNode::Seq(seq) => {
                    if !seq.is_empty() {
                        let next = seq.stms()[0];
                        *node = node.new_retain_comp(next);
                        true
                    } else {
                        node.mutate_into_next(self.env.ctx)
                    }
                }
                ControlNode::Par(par) => {
                    if par_map.contains_key(node) {
                        let count = par_map.get_mut(node).unwrap();
                        *count -= 1;
                        if *count == 0 {
                            par_map.remove(node);
                            node.mutate_into_next(self.env.ctx)
                        } else {
                            false
                        }
                    } else {
                        par_map.insert(
                            node.clone(),
                            par.stms().len().try_into().expect(
                                "More than (2^16 - 1 threads) in a par block. Are you sure this is a good idea?",
                            ),
                        );
                        new_nodes.extend(
                            par.stms().iter().map(|x| node.new_retain_comp(*x)),
                        );
                        false
                    }
                }
                ControlNode::If(i) => {
                    // this is bad but it works for now, what a headache
                    let contains_node = with_map.contains_key(node);

                     if i.cond_group().is_some() && !contains_node {
                        let comb_group = i.cond_group().unwrap();
                        let comb_assigns = ScheduledAssignments::new(node.comp, self.env.ctx.primary[comb_group].assignments, None);

                        with_map.insert(node.clone(), comb_group);

                        // TODO griffin: Sort out a way to make this error less terrible
                        // NOTE THIS MIGHT INTRODUCE A BUG SINCE THE PORTS
                        // HAVE NOT BEEN UNDEFINED YET
                        self.simulate_combinational(&[comb_assigns]).expect("something went wrong in evaluating with clause for if statement");

                        // now we fall through and proceed as normal
                    }

                    if i.cond_group().is_some() && contains_node {
                        with_map.remove(node);
                        node.mutate_into_next(self.env.ctx)
                    } else {
                        let target = GlobalPortRef::from_local(i.cond_port(), &self.env.cells[node.comp].unwrap_comp().index_bases);
                        let result = match target {
                            GlobalPortRef::Port(p) => self.env.ports[p]
                                .as_bool()
                                .expect("if condition is undefined"),
                            GlobalPortRef::Ref(r) => {
                                let index = self.env.ref_ports[r].unwrap();
                                self.env.ports[index]
                                    .as_bool()
                                    .expect("if condition is undefined")
                            }
                        };

                        let target = if result { i.tbranch() } else { i.fbranch() };
                        *node = node.new_retain_comp(target);
                        true
                    }
                }
                ControlNode::While(w) => {
                    if w.cond_group().is_some() {
                        let comb_group = with_map.entry(node.clone()).or_insert(w.cond_group().unwrap());
                        let comb_assigns = ScheduledAssignments::new(node.comp, self.env.ctx.primary[*comb_group].assignments, None);

                         // NOTE THIS MIGHT INTRODUCE A BUG SINCE THE PORTS
                         // HAVE NOT BEEN UNDEFINED YET
                        self.simulate_combinational(&[comb_assigns]).expect("something went wrong in evaluating with clause for while statement");
                    }

                    let target = GlobalPortRef::from_local(
                        w.cond_port(),
                        &self.env.cells[node.comp].unwrap_comp().index_bases,
                    );

                    let result = match target {
                        GlobalPortRef::Port(p) => self.env.ports[p]
                            .as_bool()
                            .expect("while condition is undefined"),
                        GlobalPortRef::Ref(r) => {
                            let index = self.env.ref_ports[r].unwrap();
                            self.env.ports[index]
                                .as_bool()
                                .expect("while condition is undefined")
                        }
                    };

                    if result {
                        // enter the body
                        *node = node.new_retain_comp(w.body());
                        true
                    } else {
                        if w.cond_group().is_some() {
                            with_map.remove(node);
                        }
                        // ascend the tree
                        node.mutate_into_next(self.env.ctx)
                    }
                }

                // ===== leaf nodes =====
                ControlNode::Empty(_) => node.mutate_into_next(self.env.ctx),
                ControlNode::Enable(e) => {
                    let done_local = self.env.ctx.primary[e.group()].done;
                    let done_idx = &self.env.cells[node.comp]
                        .as_comp()
                        .unwrap()
                        .index_bases
                        + done_local;

                    if !self.env.ports[done_idx].as_bool().unwrap_or_default() {
                        leaf_nodes.push(node.clone());
                        true
                    } else {
                        // This group has finished running and may be removed
                        // this is somewhat dubious at the moment since it
                        // relies on the fact that the group done port will
                        // still be high since convergence hasn't propagated the
                        // low done signal yet.
                        node.mutate_into_next(self.env.ctx)
                    }
                }
                ControlNode::Invoke(i) => {
                    let done = self.get_global_port_idx(&i.done, node.comp);

                    if i.comb_group.is_some() && !with_map.contains_key(node) {
                        with_map.insert(node.clone(), i.comb_group.unwrap());
                    }


                    if !self.env.ports[done].as_bool().unwrap_or_default() {
                        leaf_nodes.push(node.clone());
                        true
                    } else {
                        self.cleanup_ref_cells(node.comp, i);

                        if i.comb_group.is_some() {
                            with_map.remove(node);
                        }

                        node.mutate_into_next(self.env.ctx)
                    }
                },
            };

            if !retain_bool && ControlPoint::get_next(node, self.env.ctx).is_none() &&
             // either we are not a par node, or we are the last par node
             (!matches!(&self.env.ctx.primary[node.control_node_idx], ControlNode::Par(_)) || !par_map.contains_key(node)) {

                set_done.push(self.env.get_comp_done(node.comp));
                let comp_ledger = self.env.cells[node.comp].unwrap_comp();
                *node = node.new_retain_comp(self.env.ctx.primary[comp_ledger.comp_id].control.unwrap());
                true
            } else {
                retain_bool
            }

        });

        self.env.pc.restore_fields(vecs, par_map, with_map);

        // insert all the new nodes from the par into the program counter
        self.env.pc.vec_mut().extend(new_nodes);

        self.undef_all_ports();
        self.set_root_go_high();
        for port in set_done {
            self.env.ports[port] = PortValue::new_implicit(Value::bit_high());
        }

        for node in &leaf_nodes {
            match &self.env.ctx.primary[node.control_node_idx] {
                ControlNode::Enable(e) => {
                    let go_local = self.env.ctx.primary[e.group()].go;
                    let index_bases = &self.env.cells[node.comp]
                        .as_comp()
                        .unwrap()
                        .index_bases;

                    // set go high
                    let go_idx = index_bases + go_local;
                    self.env.ports[go_idx] =
                        PortValue::new_implicit(Value::bit_high());
                }
                ControlNode::Invoke(i) => {
                    let go = self.get_global_port_idx(&i.go, node.comp);
                    self.env.ports[go] =
                        PortValue::new_implicit(Value::bit_high());

                    self.intialize_ref_cells(node.comp, i);
                }
                non_leaf => {
                    unreachable!("non-leaf node {:?} included in list of leaf nodes. This should never happen, please report it.", non_leaf)
                }
            }
        }

        let assigns_bundle = self.get_assignments(&leaf_nodes);

        self.simulate_combinational(&assigns_bundle)?;

        for cell in self.env.cells.values_mut() {
            match cell {
                CellLedger::Primitive { cell_dyn } => {
                    cell_dyn.exec_cycle(&mut self.env.ports)?;
                }
                CellLedger::Component(_) => {}
            }
        }

        Ok(())
    }

    fn is_done(&self) -> bool {
        self.env.ports[self.env.get_root_done()]
            .as_bool()
            .unwrap_or_default()
    }

    /// Evaluate the entire program
    pub fn run_program(&mut self) -> InterpreterResult<()> {
        while !self.is_done() {
            self.step()?
        }
        Ok(())
    }

    fn evaluate_guard(
        &self,
        guard: GuardIdx,
        comp: GlobalCellIdx,
    ) -> Option<bool> {
        let guard = &self.ctx().primary[guard];
        match guard {
            Guard::True => Some(true),
            Guard::Or(a, b) => {
                let g1 = self.evaluate_guard(*a, comp)?;
                let g2 = self.evaluate_guard(*b, comp)?;
                Some(g1 || g2)
            }
            Guard::And(a, b) => {
                let g1 = self.evaluate_guard(*a, comp)?;
                let g2 = self.evaluate_guard(*b, comp)?;
                Some(g1 && g2)
            }
            Guard::Not(n) => Some(!self.evaluate_guard(*n, comp)?),
            Guard::Comp(c, a, b) => {
                let comp_v = self.env.cells[comp].unwrap_comp();

                let a = self
                    .lookup_global_port_id(comp_v.convert_to_global_port(a));
                let b = self
                    .lookup_global_port_id(comp_v.convert_to_global_port(b));

                let a_val = self.env.ports[a].val()?;
                let b_val = self.env.ports[b].val()?;
                match c {
                    calyx_ir::PortComp::Eq => a_val == b_val,
                    calyx_ir::PortComp::Neq => a_val != b_val,
                    calyx_ir::PortComp::Gt => a_val > b_val,
                    calyx_ir::PortComp::Lt => a_val < b_val,
                    calyx_ir::PortComp::Geq => a_val >= b_val,
                    calyx_ir::PortComp::Leq => a_val <= b_val,
                }
                .into()
            }
            Guard::Port(p) => {
                let comp_v = self.env.cells[comp].unwrap_comp();
                let p_idx = self
                    .lookup_global_port_id(comp_v.convert_to_global_port(p));
                self.env.ports[p_idx].as_bool()
            }
        }
    }

    fn undef_all_ports(&mut self) {
        for (_idx, port_val) in self.env.ports.iter_mut() {
            port_val.set_undef();
        }
    }

    fn simulate_combinational(
        &mut self,
        assigns_bundle: &[ScheduledAssignments],
    ) -> InterpreterResult<()> {
        let mut has_changed = true;

        // TODO griffin: rewrite this so that someone can actually read it
        let done_ports: Vec<_> = assigns_bundle
            .iter()
            .filter_map(|x| {
                x.interface_ports.as_ref().map(|y| {
                    &self.env.cells[x.active_cell]
                        .as_comp()
                        .unwrap()
                        .index_bases
                        + y.done
                })
            })
            .collect();

        while has_changed {
            has_changed = false;

            // evaluate all the assignments and make updates
            for ScheduledAssignments {
                active_cell,
                assignments,
                interface_ports,
            } in assigns_bundle.iter()
            {
                let ledger = self.env.cells[*active_cell].as_comp().unwrap();
                let go = interface_ports
                    .as_ref()
                    .map(|x| &ledger.index_bases + x.go);
                let done = interface_ports
                    .as_ref()
                    .map(|x| &ledger.index_bases + x.done);

                let comp_go = self.env.get_comp_go(*active_cell);

                for assign_idx in assignments {
                    let assign = &self.env.ctx.primary[assign_idx];

                    // TODO griffin: Come back to this unwrap default later
                    // since we may want to do something different if the guard
                    // does not have a defined value
                    if self
                        .evaluate_guard(assign.guard, *active_cell)
                        .unwrap_or_default()
                    // the go for the group is high
                    && go
                        .as_ref()
                        // the group must have its go signal high and the go
                        // signal of the component must also be high
                        .map(|g| self.env.ports[*g].as_bool().unwrap_or_default() && self.env.ports[comp_go].as_bool().unwrap_or_default())
                        // if there is no go signal, then we want to run the
                        // assignment
                        .unwrap_or(true)
                    {
                        let val = self.get_value(&assign.src, *active_cell);
                        let dest =
                            self.get_global_port_idx(&assign.dst, *active_cell);

                        if let Some(done) = done {
                            if dest != done {
                                let done_val = &self.env.ports[done];

                                if done_val.as_bool().unwrap_or(true) {
                                    // skip this assignment when we are done or
                                    // or the done signal is undefined
                                    continue;
                                }
                            }
                        }

                        if let Some(v) = val.as_option() {
                            let changed = self.env.ports.insert_val(
                                dest,
                                AssignedValue::new(v.val().clone(), assign_idx),
                            )?;

                            has_changed |= changed.as_bool();
                        } else if self.env.ports[dest].is_def() {
                            todo!("Raise an error here since this assignment is undefining things")
                        }
                    }
                }
            }

            // Run all the primitives
            let changed: bool = self
                .env
                .cells
                .range()
                .iter()
                .filter_map(|x| match &mut self.env.cells[x] {
                    CellLedger::Primitive { cell_dyn } => {
                        Some(cell_dyn.exec_comb(&mut self.env.ports))
                    }
                    CellLedger::Component(_) => None,
                })
                .fold_ok(UpdateStatus::Unchanged, |has_changed, update| {
                    has_changed | update
                })?
                .as_bool();

            has_changed |= changed;

            // check for undefined done ports. If any remain after we've
            // converged then they should be set to zero and we should continue
            // convergence
            if !has_changed {
                for &done_port in &done_ports {
                    if self.env.ports[done_port].is_undef() {
                        self.env.ports[done_port] =
                            PortValue::new_implicit(Value::bit_low());
                        has_changed = true;
                    }
                }
            }
        }

        Ok(())
    }

    /// Dump the current state of the environment as a DataDump
    pub fn dump_memories(&self, dump_registers: bool) -> DataDump {
        let ctx = self.ctx();
        let entrypoint_secondary = &ctx.secondary[ctx.entry_point];

        let mut dump = DataDump::new_empty_with_top_level(
            ctx.lookup_string(entrypoint_secondary.name).clone(),
        );

        let root = self.get_root_component();

        for (offset, idx) in entrypoint_secondary.cell_offset_map.iter() {
            let cell_info = &ctx.secondary[*idx];
            let cell_index = &root.index_bases + offset;
            let name = ctx.lookup_string(cell_info.name).clone();
            match &cell_info.prototype {
                CellPrototype::Memory { width, dims, .. } => dump.push_memory(
                    name,
                    *width as usize,
                    dims.size(),
                    dims.as_serializing_dim(),
                    self.env.cells[cell_index]
                        .unwrap_primitive()
                        .dump_memory_state()
                        .unwrap(),
                ),
                CellPrototype::SingleWidth {
                    op: PrimType1::Reg,
                    width,
                } => {
                    if dump_registers {
                        dump.push_memory(
                            name,
                            *width as usize,
                            1,
                            Dimensions::D1(1),
                            self.env.cells[cell_index]
                                .unwrap_primitive()
                                .dump_memory_state()
                                .unwrap(),
                        )
                    }
                }
                _ => (),
            }
        }

        dump
    }
}
