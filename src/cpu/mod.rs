mod cop0;
mod gte;

#[cfg(test)]
mod tests;

use std::fmt::{Display, Formatter, Error};
use std::default::Default;

use memory::{Interconnect, Addressable, Byte, HalfWord, Word};
use shared::SharedState;
use gpu::renderer::Renderer;
use interrupt::InterruptState;
use debugger::Debugger;
use tracer::module_tracer;

use self::cop0::{Cop0, Exception};
use self::gte::Gte;

/// This struct contains the CPU state, including the `Interconnect`
/// instance which owns most of the peripherals.
#[derive(Serialize, Deserialize)]
pub struct Cpu {
    /// The program counter register: points to the next instruction
    pc: u32,
    /// Next value for the PC, used to simulate the branch delay slot
    next_pc: u32,
    /// Address of the instruction currently being executed. Used for
    /// setting the EPC in exceptions.
    current_pc: u32,
    /// General Purpose Registers. The first entry must always contain 0
    regs: [u32; 32],
    /// HI register for division remainder and multiplication high
    /// result
    hi: u32,
    /// LO register for division quotient and multiplication low
    /// result
    lo: u32,
    /// Instruction Cache (256 4-word cachelines)
    icache: ICacheLines,
    /// Memory interface
    inter: Interconnect,
    /// Coprocessor 0: System control
    cop0: Cop0,
    /// Coprocessor 2: Geometry Transform Engine
    gte: Gte,
    /// Load initiated by the current instruction (will take effect
    /// after the load delay slot)
    load: (RegisterIndex, u32),
    /// Set by the current instruction if a branch occured and the
    /// next instruction will be in the delay slot.
    branch: bool,
    /// Set if the current instruction executes in the delay slot
    delay_slot: bool,
    /// If `true` break instructions will trigger the debugger instead
    /// of generating an exception.
    debug_on_break: bool,
}

impl Cpu {
    /// Create a new CPU instance
    pub fn new(inter: Interconnect) -> Cpu {
        // Not sure what the reset values are...
        let mut regs = [0xdeadbeef; 32];

        // ... but R0 is hardwired to 0
        regs[0] = 0;

        // Reset value for the PC, beginning of BIOS memory
        let pc = 0xbfc00000;

        Cpu {
            pc:             pc,
            next_pc:        pc.wrapping_add(4),
            current_pc:     0,
            regs:           regs,
            hi:             0xdeadbeef,
            lo:             0xdeadbeef,
            icache:         ICacheLines::new(),
            inter:          inter,
            cop0:           Cop0::new(),
            gte:            Gte::new(),
            load:           (RegisterIndex(0), 0),
            branch:         false,
            delay_slot:     false,
            debug_on_break: false,
        }
    }

    pub fn set_debug_on_break(&mut self, enabled: bool) {
        self.debug_on_break = enabled
    }

    /// Return a reference to the interconnect
    pub fn interconnect(&self) -> &Interconnect {
        &self.inter
    }

    /// Return a mutable reference to the interconnect
    pub fn interconnect_mut(&mut self) -> &mut Interconnect {
        &mut self.inter
    }

    /// Run the emulator until the start of the next frame
    pub fn run_until_next_frame<D>(&mut self,
                                   debugger: &mut D,
                                   shared: &mut SharedState,
                                   renderer: &mut Renderer)
        where D: Debugger {
        let frame = shared.counters().frame.get();

        while frame == shared.counters().frame.get() {
            self.run_next_instruction(debugger, shared, renderer);
        }
    }

    /// Run a single CPU instruction and return
    pub fn run_next_instruction<D>(&mut self,
                                   debugger: &mut D,
                                   shared: &mut SharedState,
                                   renderer: &mut Renderer)
        where D: Debugger {

        // Synchronize the peripherals
        if shared.tk().sync_pending() {
            self.inter.sync(shared);
            shared.tk().update_sync_pending();
        }

        // Save the address of the current instruction to store in
        // `EPC` in case of an exception.
        self.current_pc = self.pc;

        // Debugger entrypoint: used for code breakpoints and stepping
        debugger.pc_change(self);

        if self.current_pc % 4 != 0 {
            // PC is not correctly aligned!
            self.exception(Exception::LoadAddressError);
            return;
        }

        // Fetch instruction at PC
        let instruction = self.fetch_instruction(shared);

        // Increment PC to point to the next instruction. and
        // `next_pc` to the one after that. Both values can be
        // modified by individual instructions (`next_pc` in case of a
        // jump/branch, `pc` in case of an exception)
        self.pc         = self.next_pc;
        self.next_pc    = self.pc.wrapping_add(4);

        // If the last instruction was a branch then we're in the
        // delay slot
        self.delay_slot = self.branch;
        self.branch     = false;

        // Check for pending interrupts
        if self.cop0.irq_active(*shared.irq_state()) {
            shared.counters_mut().cpu_interrupt.increment();

            module_tracer("CPU", |m| {
                let now = shared.tk().now();

                m.trace(now,
                        "irq_count",
                        shared.counters_mut().cpu_interrupt.get());
                m.trace(now,
                        "irq_pc",
                        self.current_pc);
            });

            if instruction.is_gte_op() {
                // GTE instructions get executed even if an interrupt
                // occurs
                self.decode_and_execute(debugger,
                                        instruction,
                                        shared,
                                        renderer);
            }

            // XXX No idea how long the interrupt switch takes on the
            // real hardware?
            shared.tk().tick(1);

            self.exception(Exception::Interrupt);
        } else {
            // No interrupt pending, run the current instruction
            self.decode_and_execute(debugger, instruction, shared, renderer);
        }
    }

    /// Force the value of the PC
    pub fn set_pc(&mut self, pc: u32) {
        self.pc = pc;
        self.next_pc = pc.wrapping_add(4);
    }

    /// Fetch the instruction at `current_pc` through the instruction
    /// cache
    fn fetch_instruction(&mut self, shared: &mut SharedState) -> Instruction {
        let pc = self.current_pc;
        let cc = self.inter.cache_control();

        // KUSEG and KSEG0 regions are cached. KSEG1 is uncached and
        // KSEG2 doesn't contain any code
        let cached = pc < 0xa0000000;

        if cached && cc.icache_enabled() {
            // The MSB is ignored: running from KUSEG or KSEG0 hits
            // the same cachelines. So for instance addresses
            // 0x00000000 and 0x80000000 have the same tag and you can
            // jump from one to the other without having to reload the
            // cache.

            // Cache tag: bits [30:12]
            let tag  = pc & 0x7ffff000;
            // Cache line "bucket": bits [11:4]
            let line = (pc >> 4) & 0xff;
            // Index in the cache line: bits [3:2]
            let index = (pc >> 2) & 3;

            // Fetch the cacheline for this address
            let line = &mut self.icache[line as usize];

            // Check the tag and validity
            if line.tag() != tag || line.valid_index() > index {
                // Cache miss. Fetch the cacheline starting at the
                // current index. If the index is not 0 then some
                // words are going to remain invalid in the cacheline.
                let mut cpc = pc;

                // XXX Fetch timings from mednafen, on my console it
                // seems a bit faster than that, need to review those
                // timings when I decide to implement CPU pipelining
                // and whatnot
                shared.tk().tick(3);

                for i in index..4 {
                    shared.tk().tick(1);

                    let instruction =
                        Instruction(self.inter.load_instruction(shared, cpc));

                    line.set_instruction(i, instruction);
                    cpc += 4;
                }

                // Set the tag and valid bits
                line.set_tag_valid(pc);
            }

            // Cache line is now guaranteed to be valid
            line.instruction(index)
        } else {
            // XXX Apparently pointing the PC to KSEG2 causes a bus
            // error no matter what, even if you point it at some
            // valid register address (like the "cache control"
            // register). Not like it should happen anyway, there's
            // nowhere to put code in KSEG2, only a bunch of
            // registers.

            // Cache disabled, fetch directly from memory. Takes 4 to
            // 5 cycles on average.
            shared.tk().tick(4);

            Instruction(self.inter.load_instruction(shared, pc))
        }
    }

    /// Memory read
    fn load<A, D>(&mut self,
                  debugger: &mut D,
                  shared: &mut SharedState,
                  addr: u32) -> u32
    where A: Addressable, D: Debugger {
        debugger.memory_read(self, addr);

        self.inter.load::<A>(shared, addr)
    }

    /// Memory read with as little side-effect as possible. Used for
    /// debugging.
    pub fn examine<A: Addressable>(&mut self, addr: u32) -> u32 {

        self.inter.load::<A>(&mut SharedState::new(), addr)
    }

    /// Memory write
    ///
    /// We always pass around 32bit values even for Byte and HalfWord
    /// access because some devices ignore the requested width when
    /// writing to their registers and might use more than what we
    /// expect.
    ///
    /// On the real console the CPU always puts the entire 32bit register
    /// value on the bus so those devices might end up using all the
    /// bytes in the Word even for smaller widths.
    fn store<A, D>(&mut self,
                   debugger: &mut D,
                   shared: &mut SharedState,
                   renderer: &mut Renderer,
                   addr: u32,
                   val: u32)
    where A: Addressable, D: Debugger {
        debugger.memory_write(self, addr);

        if self.cop0.cache_isolated() {
            self.cache_maintenance::<A>(addr, val);
        } else {
            self.inter.store::<A>(shared, renderer, addr, val);
        }
    }

    /// Handle writes when the cache is isolated
    pub fn cache_maintenance<A: Addressable>(&mut self, addr: u32, val: u32) {
        // Implementing full cache emulation requires handling many
        // corner cases. For now I'm just going to add support for
        // cache invalidation which is the only use case for cache
        // isolation as far as I know.

        let cc = self.inter.cache_control();

        if !cc.icache_enabled() {
            panic!("Cache maintenance while instruction cache is disabled");
        }

        if A::size() != 4 || val != 0 {
            panic!("Unsupported write while cache is isolated: {:08x}",
                   val);
        }

        let line = (addr >> 4) & 0xff;

        // Fetch the cacheline for this address
        let line = &mut self.icache[line as usize];

        if cc.tag_test_mode() {
            // In tag test mode the write invalidates the entire
            // targeted cacheline
            line.invalidate();
        } else {
            // Otherwise the write ends up directly in the cache.
            let index = (addr >> 2) & 3;

            let instruction = Instruction(val);

            line.set_instruction(index, instruction);
        }
    }

    /// Branch to immediate value `offset`.
    fn branch(&mut self, offset: u32) {
        // Offset immediates are always shifted two places to the
        // right since `PC` addresses have to be aligned on 32bits at
        // all times.
        let offset = offset << 2;

        self.next_pc = self.pc.wrapping_add(offset);

        self.branch = true;
    }

    /// Trigger an exception
    fn exception(&mut self, cause: Exception) {

        // Update the status register
        let handler_addr =
            self.cop0.enter_exception(cause,
                                      self.current_pc,
                                      self.delay_slot);

        // Exceptions don't have a branch delay, we jump directly into
        // the handler
        self.pc      = handler_addr;
        self.next_pc = self.pc.wrapping_add(4);
    }

    /// Retrieve the value of a general purpose register
    fn reg(&self, index: RegisterIndex) -> u32 {
        self.regs[index.0 as usize]
    }

    /// Set the value of a general purpose register
    fn set_reg(&mut self, index: RegisterIndex, val: u32) {
        self.regs[index.0 as usize] = val;

        // Make sure R0 is always 0
        self.regs[0] = 0;
    }

    /// Execute any pending delayed load. Should be called *after* the
    /// input registers are read but *before* the output registers are
    /// written
    fn delayed_load(&mut self) {
        let (reg, val) = self.load;

        self.set_reg(reg, val);

        // We reset the load to target register 0 for the next
        // instruction
        self.load = (RegisterIndex(0), 0);
    }

    /// Execute the pending delayed and setup the next one. If the new
    /// load targets the same register as the current one then the
    /// older one is cancelled (i.e. it never makes it to the
    /// register).
    ///
    /// This method should be used instead of `delayed_load` for
    /// instructions that setup a delayed load.
    fn delayed_load_chain(&mut self, reg: RegisterIndex, val: u32) {
        let (pending_reg, pending_val) = self.load;

        if pending_reg != reg {
            self.set_reg(pending_reg, pending_val);
        }

        self.load = (reg, val);
    }

    /// Get the value of all general purpose registers
    pub fn regs(&self) -> &[u32] {
        &self.regs
    }

    pub fn sr(&self) -> u32 {
        self.cop0.sr()
    }

    pub fn lo(&self) -> u32 {
        self.lo
    }

    pub fn hi(&self) -> u32 {
        self.hi
    }

    pub fn pc(&self) -> u32 {
        self.pc
    }

    pub fn cause(&self, irq_state: InterruptState) -> u32 {
        self.cop0.cause(irq_state)
    }

    pub fn bad(&self) -> u32 {
        // XXX we don't emulate the "BAD" cop0 register yet. It's
        // almost useless in the PSX anyway since there's no MMU.
        0
    }

    /// Force PC address. Meant to be used from the debugger. Use at
    /// your own risk.
    pub fn force_pc(&mut self, pc: u32) {
        self.pc = pc;
        self.next_pc = self.pc.wrapping_add(4);
        self.delay_slot = false;
    }

    /// Decode `instruction`'s opcode and run the function
    fn decode_and_execute<D>(&mut self,
                             debugger: &mut D,
                             instruction: Instruction,
                             shared: &mut SharedState,
                             renderer: &mut Renderer)
        where D: Debugger {
        // Simulate instruction execution time.
        shared.tk().tick(1);

        match instruction.function() {
            0b000000 => match instruction.subfunction() {
                0b000000 => self.op_sll(instruction),
                0b000010 => self.op_srl(instruction),
                0b000011 => self.op_sra(instruction),
                0b000100 => self.op_sllv(instruction),
                0b000110 => self.op_srlv(instruction),
                0b000111 => self.op_srav(instruction),
                0b001000 => self.op_jr(instruction),
                0b001001 => self.op_jalr(instruction),
                0b001100 => self.op_syscall(instruction),
                0b001101 => self.op_break(instruction, debugger),
                0b010000 => self.op_mfhi(instruction),
                0b010001 => self.op_mthi(instruction),
                0b010010 => self.op_mflo(instruction),
                0b010011 => self.op_mtlo(instruction),
                0b011000 => self.op_mult(instruction),
                0b011001 => self.op_multu(instruction),
                0b011010 => self.op_div(instruction),
                0b011011 => self.op_divu(instruction),
                0b100000 => self.op_add(instruction),
                0b100001 => self.op_addu(instruction),
                0b100010 => self.op_sub(instruction),
                0b100011 => self.op_subu(instruction),
                0b100100 => self.op_and(instruction),
                0b100101 => self.op_or(instruction),
                0b100110 => self.op_xor(instruction),
                0b100111 => self.op_nor(instruction),
                0b101010 => self.op_slt(instruction),
                0b101011 => self.op_sltu(instruction),
                _        => self.op_illegal(instruction),
            },
            0b000001 => self.op_bxx(instruction),
            0b000010 => self.op_j(instruction),
            0b000011 => self.op_jal(instruction),
            0b000100 => self.op_beq(instruction),
            0b000101 => self.op_bne(instruction),
            0b000110 => self.op_blez(instruction),
            0b000111 => self.op_bgtz(instruction),
            0b001000 => self.op_addi(instruction),
            0b001001 => self.op_addiu(instruction),
            0b001010 => self.op_slti(instruction),
            0b001011 => self.op_sltiu(instruction),
            0b001100 => self.op_andi(instruction),
            0b001101 => self.op_ori(instruction),
            0b001110 => self.op_xori(instruction),
            0b001111 => self.op_lui(instruction),
            0b010000 => self.op_cop0(instruction, shared),
            0b010001 => self.op_cop1(instruction),
            0b010010 => self.op_cop2(instruction),
            0b010011 => self.op_cop3(instruction),
            0b100000 => self.op_lb(instruction, debugger, shared),
            0b100001 => self.op_lh(instruction, debugger, shared),
            0b100010 => self.op_lwl(instruction, debugger, shared),
            0b100011 => self.op_lw(instruction, debugger, shared),
            0b100100 => self.op_lbu(instruction, debugger, shared),
            0b100101 => self.op_lhu(instruction, debugger, shared),
            0b100110 => self.op_lwr(instruction, debugger, shared),
            0b101000 => self.op_sb(instruction, debugger, shared, renderer),
            0b101001 => self.op_sh(instruction, debugger, shared, renderer),
            0b101010 => self.op_swl(instruction, debugger, shared, renderer),
            0b101011 => self.op_sw(instruction, debugger, shared, renderer),
            0b101110 => self.op_swr(instruction, debugger, shared, renderer),
            0b110000 => self.op_lwc0(instruction),
            0b110001 => self.op_lwc1(instruction),
            0b110010 => self.op_lwc2(instruction, debugger, shared),
            0b110011 => self.op_lwc3(instruction),
            0b111000 => self.op_swc0(instruction),
            0b111001 => self.op_swc1(instruction),
            0b111010 => self.op_swc2(instruction, debugger, shared, renderer),
            0b111011 => self.op_swc3(instruction),
            _        => self.op_illegal(instruction),
        }
    }

    /// Illegal instruction
    fn op_illegal(&mut self, instruction: Instruction) {
        self.delayed_load();

        warn!("Illegal instruction {} at PC 0x{:08x}!",
              instruction,
              self.current_pc);

        self.exception(Exception::IllegalInstruction);
    }

    /// Shift Left Logical
    fn op_sll(&mut self, instruction: Instruction) {
        let i = instruction.shift();
        let t = instruction.t();
        let d = instruction.d();

        let v = self.reg(t) << i;

        self.delayed_load();

        self.set_reg(d, v);
    }

    /// Shift Right Logical
    fn op_srl(&mut self, instruction: Instruction) {
        let i = instruction.shift();
        let t = instruction.t();
        let d = instruction.d();

        let v = self.reg(t) >> i;

        self.delayed_load();

        self.set_reg(d, v);
    }

    /// Shift Right Arithmetic
    fn op_sra(&mut self, instruction: Instruction) {
        let i = instruction.shift();
        let t = instruction.t();
        let d = instruction.d();

        let v = (self.reg(t) as i32) >> i;

        self.delayed_load();

        self.set_reg(d, v as u32);
    }

    /// Shift Left Logical Variable
    fn op_sllv(&mut self, instruction: Instruction) {
        let d = instruction.d();
        let s = instruction.s();
        let t = instruction.t();

        // Shift amount is truncated to 5 bits
        let v = self.reg(t) << (self.reg(s) & 0x1f);

        self.delayed_load();

        self.set_reg(d, v);
    }

    /// Shift Right Logical Variable
    fn op_srlv(&mut self, instruction: Instruction) {
        let d = instruction.d();
        let s = instruction.s();
        let t = instruction.t();

        // Shift amount is truncated to 5 bits
        let v = self.reg(t) >> (self.reg(s) & 0x1f);

        self.delayed_load();

        self.set_reg(d, v);
    }

    /// Shift Right Arithmetic Variable
    fn op_srav(&mut self, instruction: Instruction) {
        let d = instruction.d();
        let s = instruction.s();
        let t = instruction.t();

        // Shift amount is truncated to 5 bits
        let v = (self.reg(t) as i32) >> (self.reg(s) & 0x1f);

        self.delayed_load();

        self.set_reg(d, v as u32);
    }

    /// Various branch instructions: BGEZ, BLTZ, BGEZAL, BLTZAL. Bits
    /// [20:16] are used to figure out which one to use
    fn op_bxx(&mut self, instruction: Instruction) {
        let i = instruction.imm_se();
        let s = instruction.s();

        let instruction = instruction.0;

        let is_bgez = (instruction >> 16) & 1;
        // It's not enough to test for bit 20 to see if we're supposed
        // to link, if any bit in the range [19:17] is set the link
        // doesn't take place and RA is left untouched.
        let is_link = (instruction >> 17) & 0xf == 0x8;

        let v = self.reg(s) as i32;

        // Test "less than zero"
        let test = (v < 0) as u32;

        // If the test is "greater than or equal to zero" we need to
        // negate the comparison above ("a >= 0" <=> "!(a < 0)"). The
        // xor takes care of that.
        let test = test ^ is_bgez;

        self.delayed_load();

        // If linking is requested it occurs unconditionally, even if
        // the branch is not taken
        if is_link {
            let ra = self.next_pc;

            // Store return address in R31
            self.set_reg(RegisterIndex(31), ra);
        }

        if test != 0 {
            self.branch(i);
        }
    }

    /// Jump Register
    fn op_jr(&mut self, instruction: Instruction) {
        let s = instruction.s();

        self.next_pc = self.reg(s);

        self.delayed_load();

        self.branch = true;
    }

    /// Jump And Link Register
    fn op_jalr(&mut self, instruction: Instruction) {
        let d = instruction.d();
        let s = instruction.s();

        let ra = self.next_pc;

        self.next_pc = self.reg(s);

        self.delayed_load();

        // Store return address in `d`
        self.set_reg(d, ra);

        self.branch = true;
    }

    /// System Call
    fn op_syscall(&mut self, _: Instruction) {
        self.delayed_load();

        self.exception(Exception::SysCall);
    }

    /// Break
    fn op_break<D: Debugger>(&mut self,
                             _: Instruction,
                             debugger: &mut D) {
        self.delayed_load();

        if self.debug_on_break {
            info!("BREAK instruction while debug_on_break is active");
            debugger.trigger_break();
        } else {
            self.exception(Exception::Break);
        }
    }

    /// Move From HI
    fn op_mfhi(&mut self, instruction: Instruction) {
        let d = instruction.d();

        let hi = self.hi;

        self.delayed_load();

        self.set_reg(d, hi);
    }

    /// Move to HI
    fn op_mthi(&mut self, instruction: Instruction) {
        let s = instruction.s();

        self.hi = self.reg(s);

        self.delayed_load();
    }

    /// Move From LO
    fn op_mflo(&mut self, instruction: Instruction) {
        let d = instruction.d();

        let lo = self.lo;

        self.delayed_load();

        self.set_reg(d, lo);
    }

    /// Move to LO
    fn op_mtlo(&mut self, instruction: Instruction) {
        let s = instruction.s();

        self.lo = self.reg(s);

        self.delayed_load();
    }

    /// Multiply (signed)
    fn op_mult(&mut self, instruction: Instruction) {
        let s = instruction.s();
        let t = instruction.t();

        let a = (self.reg(s) as i32) as i64;
        let b = (self.reg(t) as i32) as i64;

        self.delayed_load();

        let v = (a * b) as u64;

        self.hi = (v >> 32) as u32;
        self.lo = v as u32;
    }

    /// Multiply Unsigned
    fn op_multu(&mut self, instruction: Instruction) {
        let s = instruction.s();
        let t = instruction.t();

        let a = self.reg(s) as u64;
        let b = self.reg(t) as u64;

        self.delayed_load();

        let v = a * b;

        self.hi = (v >> 32) as u32;
        self.lo = v as u32;
    }

    /// Divide (signed)
    fn op_div(&mut self, instruction: Instruction) {
        let s = instruction.s();
        let t = instruction.t();

        let n = self.reg(s) as i32;
        let d = self.reg(t) as i32;

        self.delayed_load();

        if d == 0 {
            // Division by zero, results are bogus
            self.hi = n as u32;

            if n >= 0 {
                self.lo = 0xffffffff;
            } else {
                self.lo = 1;
            }
        } else if n as u32 == 0x80000000 && d == -1 {
            // Result is not representable in a 32bit signed integer
            self.hi = 0;
            self.lo = 0x80000000;
        } else {
            self.hi = (n % d) as u32;
            self.lo = (n / d) as u32;
        }
    }

    /// Divide Unsigned
    fn op_divu(&mut self, instruction: Instruction) {
        let s = instruction.s();
        let t = instruction.t();

        let n = self.reg(s);
        let d = self.reg(t);

        self.delayed_load();

        if d == 0 {
            // Division by zero, results are bogus
            self.hi = n;
            self.lo = 0xffffffff;
        } else {
            self.hi = n % d;
            self.lo = n / d;
        }
    }

    /// Add and check for signed overflow
    fn op_add(&mut self, instruction: Instruction) {
        let s = instruction.s();
        let t = instruction.t();
        let d = instruction.d();

        let s = self.reg(s) as i32;
        let t = self.reg(t) as i32;

        self.delayed_load();

        match s.checked_add(t) {
            Some(v) => self.set_reg(d, v as u32),
            None    => self.exception(Exception::Overflow),
        }
    }

    /// Add Unsigned
    fn op_addu(&mut self, instruction: Instruction) {
        let s = instruction.s();
        let t = instruction.t();
        let d = instruction.d();

        let v = self.reg(s).wrapping_add(self.reg(t));

        self.delayed_load();

        self.set_reg(d, v);
    }

    /// Substract and check for signed overflow
    fn op_sub(&mut self, instruction: Instruction) {
        let s = instruction.s();
        let t = instruction.t();
        let d = instruction.d();

        let s = self.reg(s) as i32;
        let t = self.reg(t) as i32;

        self.delayed_load();

        match s.checked_sub(t) {
            Some(v) => self.set_reg(d, v as u32),
            None    => self.exception(Exception::Overflow),
        }
    }

    /// Substract Unsigned
    fn op_subu(&mut self, instruction: Instruction) {
        let s = instruction.s();
        let t = instruction.t();
        let d = instruction.d();

        let v = self.reg(s).wrapping_sub(self.reg(t));

        self.delayed_load();

        self.set_reg(d, v);
    }

    /// Bitwise And
    fn op_and(&mut self, instruction: Instruction) {
        let d = instruction.d();
        let s = instruction.s();
        let t = instruction.t();

        let v = self.reg(s) & self.reg(t);

        self.delayed_load();

        self.set_reg(d, v);
    }

    /// Bitwise Or
    fn op_or(&mut self, instruction: Instruction) {
        let d = instruction.d();
        let s = instruction.s();
        let t = instruction.t();

        let v = self.reg(s) | self.reg(t);

        self.delayed_load();

        self.set_reg(d, v);
    }

    /// Bitwise Exclusive Or
    fn op_xor(&mut self, instruction: Instruction) {
        let d = instruction.d();
        let s = instruction.s();
        let t = instruction.t();

        let v = self.reg(s) ^ self.reg(t);

        self.delayed_load();

        self.set_reg(d, v);
    }

    /// Bitwise Not Or
    fn op_nor(&mut self, instruction: Instruction) {
        let d = instruction.d();
        let s = instruction.s();
        let t = instruction.t();

        let v = !(self.reg(s) | self.reg(t));

        self.delayed_load();

        self.set_reg(d, v);
    }

    /// Set on Less Than (signed)
    fn op_slt(&mut self, instruction: Instruction) {
        let d = instruction.d();
        let s = instruction.s();
        let t = instruction.t();

        let s = self.reg(s) as i32;
        let t = self.reg(t) as i32;

        self.delayed_load();

        let v = s < t;

        self.set_reg(d, v as u32);
    }

    /// Set on Less Than Unsigned
    fn op_sltu(&mut self, instruction: Instruction) {
        let d = instruction.d();
        let s = instruction.s();
        let t = instruction.t();

        let v = self.reg(s) < self.reg(t);

        self.delayed_load();

        self.set_reg(d, v as u32);
    }

    /// Jump
    fn op_j(&mut self, instruction: Instruction) {
        let i = instruction.imm_jump();

        self.next_pc = (self.pc & 0xf0000000) | (i << 2);

        self.branch = true;

        self.delayed_load();
    }

    /// Jump And Link
    fn op_jal(&mut self, instruction: Instruction) {
        let ra = self.next_pc;

        self.op_j(instruction);

        // Store return address in R31
        self.set_reg(RegisterIndex(31), ra);

        self.branch = true;
    }

    /// Branch if Equal
    fn op_beq(&mut self, instruction: Instruction) {
        let i = instruction.imm_se();
        let s = instruction.s();
        let t = instruction.t();

        if self.reg(s) == self.reg(t) {
            self.branch(i);
        }

        self.delayed_load();
    }

    /// Branch if Not Equal
    fn op_bne(&mut self, instruction: Instruction) {
        let i = instruction.imm_se();
        let s = instruction.s();
        let t = instruction.t();

        if self.reg(s) != self.reg(t) {
            self.branch(i);
        }

        self.delayed_load();
    }

    /// Branch if Less than or Equal to Zero
    fn op_blez(&mut self, instruction: Instruction) {
        let i = instruction.imm_se();
        let s = instruction.s();

        let v = self.reg(s) as i32;

        if v <= 0 {
            self.branch(i);
        }

        self.delayed_load();
    }

    /// Branch if Greater Than Zero
    fn op_bgtz(&mut self, instruction: Instruction) {
        let i = instruction.imm_se();
        let s = instruction.s();

        let v = self.reg(s) as i32;

        if v > 0 {
            self.branch(i);
        }

        self.delayed_load();
    }

    /// Add Immediate and check for signed overflow
    fn op_addi(&mut self, instruction: Instruction) {
        let i = instruction.imm_se() as i32;
        let t = instruction.t();
        let s = instruction.s();

        let s = self.reg(s) as i32;

        self.delayed_load();

        match s.checked_add(i) {
            Some(v) => self.set_reg(t, v as u32),
            None    => self.exception(Exception::Overflow),
        }
    }

    /// Add Immediate Unsigned
    fn op_addiu(&mut self, instruction: Instruction) {
        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let v = self.reg(s).wrapping_add(i);

        self.delayed_load();

        self.set_reg(t, v);
    }

    /// Set if Less Than Immediate (signed)
    fn op_slti(&mut self, instruction: Instruction) {
        let i = instruction.imm_se() as i32;
        let s = instruction.s();
        let t = instruction.t();

        let v = (self.reg(s) as i32) < i;

        self.delayed_load();

        self.set_reg(t, v as u32);
    }

    /// Set if Less Than Immediate Unsigned
    fn op_sltiu(&mut self, instruction: Instruction) {
        let i = instruction.imm_se();
        let s = instruction.s();
        let t = instruction.t();

        let v = self.reg(s) < i;

        self.delayed_load();

        self.set_reg(t, v as u32);
    }

    /// Bitwise And Immediate
    fn op_andi(&mut self, instruction: Instruction) {
        let i = instruction.imm();
        let t = instruction.t();
        let s = instruction.s();

        let v = self.reg(s) & i;

        self.delayed_load();

        self.set_reg(t, v);
    }

    /// Bitwise Or Immediate
    fn op_ori(&mut self, instruction: Instruction) {
        let i = instruction.imm();
        let t = instruction.t();
        let s = instruction.s();

        let v = self.reg(s) | i;

        self.delayed_load();

        self.set_reg(t, v);
    }

    /// Bitwise eXclusive Or Immediate
    fn op_xori(&mut self, instruction: Instruction) {
        let i = instruction.imm();
        let t = instruction.t();
        let s = instruction.s();

        let v = self.reg(s) ^ i;

        self.delayed_load();

        self.set_reg(t, v);
    }

    /// Load Upper Immediate
    fn op_lui(&mut self, instruction: Instruction) {
        let i = instruction.imm();
        let t = instruction.t();

        // Low 16bits are set to 0
        let v = i << 16;

        self.delayed_load();

        self.set_reg(t, v);
    }

    /// Coprocessor 0 opcode
    fn op_cop0(&mut self, instruction: Instruction, shared: &mut SharedState) {
        match instruction.cop_opcode() {
            0b00000 => self.op_mfc0(instruction, shared),
            0b00100 => self.op_mtc0(instruction),
            0b10000 => self.op_rfe(instruction),
            _       => panic!("unhandled cop0 instruction {}", instruction)
        }
    }

    /// Move From Coprocessor 0
    fn op_mfc0(&mut self, instruction: Instruction, shared: &mut SharedState) {
        let cpu_r = instruction.t();
        let cop_r = instruction.d().0;

        let v = match cop_r {
            6 => {
                // No$ says this register "randomly" memorizes a jump
                // target after certain exceptions occur. Doesn't seem
                // very useful and would require a lot more testing to
                // implement accurately.
                warn!("Unhandled read from JUMP_DEST (cop0r6)");
                0
            }
            7 => {
                // DCIC: breakpoint control
                warn!("Unhandled read from DCIC (cop0r7)");
                0
            }
            8 => {
                // This register should be mostly useless on the
                // PlayStation since it doesn't have virtual memory,
                // however some exceptions do write to this register
                // so maybe we'll have to implement this correctly
                // some day.
                warn!("Unhandled read from BAD_VADDR (cop0r8)");
                0
            }
            12 => self.cop0.sr(),
            13 => self.cop0.cause(*shared.irq_state()),
            14 => self.cop0.epc(),
            15 => PROCESSOR_ID,
            _  => panic!("Unhandled read from cop0r{}", cop_r),
        };

        self.delayed_load_chain(cpu_r, v);
    }

    /// Move To Coprocessor 0
    fn op_mtc0(&mut self, instruction: Instruction) {
        let cpu_r = instruction.t();
        let cop_r = instruction.d().0;

        let v = self.reg(cpu_r);

        self.delayed_load();

        match cop_r {
            3 | 5 | 6 | 7 | 9 | 11  => // Breakpoints registers
                if v != 0 {
                    panic!("Unhandled write to cop0r{}: {:08x}", cop_r, v)
                },
            12 => self.cop0.set_sr(v),
            13 => self.cop0.set_cause(v),
            _  => panic!("Unhandled cop0 register {}", cop_r),
        }
    }

    /// Return From Exception
    fn op_rfe(&mut self, instruction: Instruction) {
        self.delayed_load();

        // There are other instructions with the same encoding but all
        // are virtual memory related and the PlayStation doesn't
        // implement them. Still, let's make sure we're not running
        // buggy code.
        if instruction.0 & 0x3f != 0b010000 {
            panic!("Invalid cop0 instruction: {}", instruction);
        }

        self.cop0.return_from_exception();
    }

    /// Coprocessor 1 opcode (does not exist on the PlayStation)
    fn op_cop1(&mut self, _: Instruction) {
        self.delayed_load();

        self.exception(Exception::CoprocessorError);
    }

    /// Coprocessor 2 opcode (GTE)
    fn op_cop2(&mut self, instruction: Instruction) {
        // XXX: we should check that the GTE is enabled in cop0's
        // status register, otherwise the cop2 instructions seem to
        // freeze the CPU (or maybe raise an exception?). Furthermore
        // it seems that one has to wait at least two cycles (tested
        // with two nops) after raising the flag in the status
        // register before the GTE can be accessed.
        let cop_opcode = instruction.cop_opcode();

        if cop_opcode & 0x10 != 0 {
            // GTE command
            // XXX handle GTE command duration
            self.gte.command(instruction.0);
        } else {
            match cop_opcode {
                0b00000 => self.op_mfc2(instruction),
                0b00010 => self.op_cfc2(instruction),
                0b00100 => self.op_mtc2(instruction),
                0b00110 => self.op_ctc2(instruction),
                _       => panic!("unhandled GTE instruction {}", instruction),
            }
        }
    }

    /// Move From Coprocessor 2 Data register
    fn op_mfc2(&mut self, instruction: Instruction) {
        let cpu_r = instruction.t();
        let cop_r = instruction.d().0;

        let v = self.gte.data(cop_r);

        self.delayed_load_chain(cpu_r, v);
    }

    /// Move From Coprocessor 2 Control register
    fn op_cfc2(&mut self, instruction: Instruction) {
        let cpu_r = instruction.t();
        let cop_r = instruction.d().0;

        let v = self.gte.control(cop_r);

        self.delayed_load_chain(cpu_r, v);
    }

    /// Move To Coprocessor 2 Data register
    fn op_mtc2(&mut self, instruction: Instruction) {
        let cpu_r = instruction.t();
        let cop_r = instruction.d().0;

        let v = self.reg(cpu_r);

        self.delayed_load();

        self.gte.set_data(cop_r, v);
    }


    /// Move To Coprocessor 2 Control register
    fn op_ctc2(&mut self, instruction: Instruction) {
        let cpu_r = instruction.t();
        let cop_r = instruction.d().0;

        let v = self.reg(cpu_r);

        self.delayed_load();

        self.gte.set_control(cop_r, v);
    }

    /// Coprocessor 3 opcode (does not exist on the PlayStation)
    fn op_cop3(&mut self, _: Instruction) {
        self.delayed_load();

        self.exception(Exception::CoprocessorError);
    }

    /// Load Byte (signed)
    fn op_lb<D: Debugger>(&mut self,
                          instruction: Instruction,
                          debugger: &mut D,
                          shared: &mut SharedState) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);

        // Cast as i8 to force sign extension
        let v = self.load::<Byte, D>(debugger, shared, addr) as i8;

        self.delayed_load_chain(t, v as u32);
    }

    /// Load Halfword (signed)
    fn op_lh<D: Debugger>(&mut self,
                          instruction: Instruction,
                          debugger: &mut D,
                          shared: &mut SharedState) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);

        // Address must be 16bit aligned
        if addr % 2 == 0 {
            // Cast as i16 to force sign extension
            let v = self.load::<HalfWord, D>(debugger, shared, addr) as i16;

            self.delayed_load_chain(t, v as u32);
        } else {
            self.delayed_load();
            self.exception(Exception::LoadAddressError);
        }
    }

    /// Load Word Left (little-endian only implementation)
    fn op_lwl<D: Debugger>(&mut self,
                           instruction: Instruction,
                           debugger: &mut D,
                           shared: &mut SharedState) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);

        // This instruction bypasses the load delay restriction: this
        // instruction will merge the new contents with the value
        // currently being loaded if need be.
        let (pending_reg, pending_value) = self.load;

        let cur_v =
            if pending_reg == t {
                pending_value
            } else {
                self.reg(t)
            };

        // Next we load the *aligned* word containing the first
        // addressed byte
        let aligned_addr = addr & !3;
        let aligned_word = self.load::<Word, D>(debugger, shared, aligned_addr);

        // Depending on the address alignment we fetch the 1, 2, 3 or
        // 4 *most* significant bytes and put them in the target
        // register.
        let v = match addr & 3 {
            0 => (cur_v & 0x00ffffff) | (aligned_word << 24),
            1 => (cur_v & 0x0000ffff) | (aligned_word << 16),
            2 => (cur_v & 0x000000ff) | (aligned_word << 8),
            3 => (cur_v & 0x00000000) | (aligned_word << 0),
            _ => unreachable!(),
        };

        self.delayed_load_chain(t, v);
    }

    /// Load Word
    fn op_lw<D: Debugger>(&mut self,
                          instruction: Instruction,
                          debugger: &mut D,
                          shared: &mut SharedState) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);

        // Address must be 32bit aligned
        if addr % 4 == 0 {
            let v = self.load::<Word, D>(debugger, shared, addr);

            self.delayed_load_chain(t, v);
        } else {
            self.delayed_load();
            self.exception(Exception::LoadAddressError);
        }
    }

    /// Load Byte Unsigned
    fn op_lbu<D: Debugger>(&mut self,
                           instruction: Instruction,
                           debugger: &mut D,
                           shared: &mut SharedState) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);

        let v = self.load::<Byte, D>(debugger, shared, addr);

        self.delayed_load_chain(t, v as u32);
    }

    /// Load Halfword Unsigned
    fn op_lhu<D: Debugger>(&mut self,
                           instruction: Instruction,
                           debugger: &mut D,
                           shared: &mut SharedState) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);

        // Address must be 16bit aligned
        if addr % 2 == 0 {
            let v = self.load::<HalfWord, D>(debugger, shared, addr);

            self.delayed_load_chain(t, v);
        } else {
            self.delayed_load();
            self.exception(Exception::LoadAddressError);
        }
    }

    /// Load Word Right (little-endian only implementation)
    fn op_lwr<D: Debugger>(&mut self,
                           instruction: Instruction,
                           debugger: &mut D,
                           shared: &mut SharedState) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);

        // This instruction bypasses the load delay restriction: this
        // instruction will merge the new contents with the value
        // currently being loaded if need be.
        let (pending_reg, pending_value) = self.load;

        let cur_v =
            if pending_reg == t {
                pending_value
            } else {
                self.reg(t)
            };

        // Next we load the *aligned* word containing the first
        // addressed byte
        let aligned_addr = addr & !3;
        let aligned_word = self.load::<Word, D>(debugger, shared, aligned_addr);

        // Depending on the address alignment we fetch the 1, 2, 3 or
        // 4 *least* significant bytes and put them in the target
        // register.
        let v = match addr & 3 {
            0 => (cur_v & 0x00000000) | (aligned_word >> 0),
            1 => (cur_v & 0xff000000) | (aligned_word >> 8),
            2 => (cur_v & 0xffff0000) | (aligned_word >> 16),
            3 => (cur_v & 0xffffff00) | (aligned_word >> 24),
            _ => unreachable!(),
        };

        // Put the load in the delay slot
        self.delayed_load_chain(t, v);
    }

    /// Store Byte
    fn op_sb<D: Debugger>(&mut self,
                          instruction: Instruction,
                          debugger: &mut D,
                          shared: &mut SharedState,
                          renderer: &mut Renderer) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);
        let v    = self.reg(t);

        self.delayed_load();

        self.store::<Byte, D>(debugger, shared, renderer, addr, v);
    }

    /// Store Halfword
    fn op_sh<D: Debugger>(&mut self,
                          instruction: Instruction,
                          debugger: &mut D,
                          shared: &mut SharedState,
                          renderer: &mut Renderer) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);
        let v    = self.reg(t);

        self.delayed_load();

        // Address must be 16bit aligned
        if addr % 2 == 0 {
            self.store::<HalfWord, D>(debugger, shared, renderer, addr, v);
        } else {
            self.exception(Exception::StoreAddressError);
        }
    }

    /// Store Word Left (little-endian only implementation)
    fn op_swl<D: Debugger>(&mut self,
                           instruction: Instruction,
                           debugger: &mut D,
                           shared: &mut SharedState,
                           renderer: &mut Renderer) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);
        let v    = self.reg(t);

        let aligned_addr = addr & !3;
        // Load the current value for the aligned word at the target
        // address
        let cur_mem = self.load::<Word, D>(debugger, shared, aligned_addr);

        let mem =
            match addr & 3 {
                0 => (cur_mem & 0xffffff00) | (v >> 24),
                1 => (cur_mem & 0xffff0000) | (v >> 16),
                2 => (cur_mem & 0xff000000) | (v >> 8),
                3 => (cur_mem & 0x00000000) | (v >> 0),
                _ => unreachable!(),
            };

        self.delayed_load();

        self.store::<Word, D>(debugger, shared, renderer, aligned_addr, mem);
    }

    /// Store Word
    fn op_sw<D: Debugger>(&mut self,
                          instruction: Instruction,
                          debugger: &mut D,
                          shared: &mut SharedState,
                          renderer: &mut Renderer) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);
        let v    = self.reg(t);

        self.delayed_load();

        // Address must be 32bit aligned
        if addr % 4 == 0 {
            self.store::<Word, D>(debugger, shared, renderer, addr, v);
        } else {
            self.exception(Exception::StoreAddressError);
        }
    }

    /// Store Word Right (little-endian only implementation)
    fn op_swr<D: Debugger>(&mut self,
                           instruction: Instruction,
                           debugger: &mut D,
                           shared: &mut SharedState,
                           renderer: &mut Renderer) {

        let i = instruction.imm_se();
        let t = instruction.t();
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);
        let v    = self.reg(t);

        let aligned_addr = addr & !3;
        // Load the current value for the aligned word at the target
        // address
        let cur_mem = self.load::<Word, D>(debugger, shared, aligned_addr);

        let mem =
            match addr & 3 {
                0 => (cur_mem & 0x00000000) | (v << 0),
                1 => (cur_mem & 0x000000ff) | (v << 8),
                2 => (cur_mem & 0x0000ffff) | (v << 16),
                3 => (cur_mem & 0x00ffffff) | (v << 24),
                _ => unreachable!(),
        };

        self.delayed_load();

        self.store::<Word, D>(debugger, shared, renderer, aligned_addr, mem);
    }

    /// Load Word in Coprocessor 0
    fn op_lwc0(&mut self, _: Instruction) {
        self.delayed_load();

        // Not supported by this coprocessor
        self.exception(Exception::CoprocessorError);
    }

    /// Load Word in Coprocessor 1
    fn op_lwc1(&mut self, _: Instruction) {
        self.delayed_load();

        // Not supported by this coprocessor
        self.exception(Exception::CoprocessorError);
    }

    /// Load Word in Coprocessor 2
    fn op_lwc2<D: Debugger>(&mut self,
                            instruction: Instruction,
                            debugger: &mut D,
                            shared: &mut SharedState) {

        let i = instruction.imm_se();
        let cop_r = instruction.t().0;
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);

        self.delayed_load();

        // Address must be 32bit aligned
        if addr % 4 == 0 {
            let v = self.load::<Word, D>(debugger, shared, addr);

            // Send to coprocessor
            self.gte.set_data(cop_r, v);
        } else {
            self.exception(Exception::LoadAddressError);
        }
    }

    /// Load Word in Coprocessor 3
    fn op_lwc3(&mut self, _: Instruction) {
        self.delayed_load();

        // Not supported by this coprocessor
        self.exception(Exception::CoprocessorError);
    }

    /// Store Word in Coprocessor 0
    fn op_swc0(&mut self, _: Instruction) {
        self.delayed_load();

        // Not supported by this coprocessor
        self.exception(Exception::CoprocessorError);
    }

    /// Store Word in Coprocessor 1
    fn op_swc1(&mut self, _: Instruction) {
        self.delayed_load();

        // Not supported by this coprocessor
        self.exception(Exception::CoprocessorError);
    }

    /// Store Word in Coprocessor 2
    fn op_swc2<D: Debugger>(&mut self,
                            instruction: Instruction,
                            debugger: &mut D,
                            shared: &mut SharedState,
                            renderer: &mut Renderer) {
        let i = instruction.imm_se();
        let cop_r = instruction.t().0;
        let s = instruction.s();

        let addr = self.reg(s).wrapping_add(i);
        let v = self.gte.data(cop_r);

        self.delayed_load();

        // Address must be 32bit aligned
        if addr % 4 == 0 {
            self.store::<Word, D>(debugger, shared, renderer, addr, v);
        } else {
            self.exception(Exception::LoadAddressError);
        }
    }

    /// Store Word in Coprocessor 3
    fn op_swc3(&mut self, _: Instruction) {
        self.delayed_load();

        // Not supported by this coprocessor
        self.exception(Exception::CoprocessorError);
    }
}

/// Simple wrapper around an instruction word to provide type-safety.
#[derive(Clone, Copy, Serialize, Deserialize)]
struct Instruction(u32);

impl Instruction {
    /// Return bits [31:26] of the instruction
    fn function(self) -> u32 {
        let Instruction(op) = self;

        op >> 26
    }

    /// Return bits [5:0] of the instruction
    fn subfunction(self) -> u32 {
        let Instruction(op) = self;

        op & 0x3f
    }

    /// Return coprocessor opcode in bits [25:21]
    fn cop_opcode(self) -> u32 {
        let Instruction(op) = self;

        (op >> 21) & 0x1f
    }

    /// Return register index in bits [25:21]
    fn s(self) -> RegisterIndex {
        let Instruction(op) = self;

        RegisterIndex((op >> 21) & 0x1f)
    }

    /// Return register index in bits [20:16]
    fn t(self) -> RegisterIndex {
        let Instruction(op) = self;

        RegisterIndex((op >> 16) & 0x1f)
    }

    /// Return register index in bits [15:11]
    fn d(self) -> RegisterIndex {
        let Instruction(op) = self;

        RegisterIndex((op >> 11) & 0x1f)
    }

    /// Return immediate value in bits [16:0]
    fn imm(self) -> u32 {
        let Instruction(op) = self;

        op & 0xffff
    }

    /// Return immediate value in bits [16:0] as a sign-extended 32bit
    /// value
    fn imm_se(self) -> u32 {
        let Instruction(op) = self;

        let v = (op & 0xffff) as i16;

        v as u32
    }

    /// Shift Immediate values are stored in bits [10:6]
    fn shift(self) -> u32 {
        let Instruction(op) = self;

        (op >> 6) & 0x1f
    }

    /// Jump target stored in bits [25:0]
    fn imm_jump(self) -> u32 {
        let Instruction(op) = self;

        op & 0x3ffffff
    }

    /// Return true if the instruction contains a GTE/COP2 opcode
    fn is_gte_op(self) -> bool {
        // XXX This will match all GTE instructions including mfc/mtc
        // and friends, do we only want to match GTE operations
        // instead?
        self.function() == 0b010001
    }
}

impl Display for Instruction {
    fn fmt(&self, f: &mut Formatter) -> Result<(), Error> {
        write!(f, "{:08x}", self.0)
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct RegisterIndex(u32);

/// Instruction cache line
#[derive(Clone, Copy, Serialize, Deserialize)]
struct ICacheLine {
    /// Tag: high 22bits of the address associated with this cacheline
    /// Valid bits: 3 bit index of the first valid word in line.
    tag_valid: u32,
    /// Four words per line
    line: [Instruction; 4],
}

impl ICacheLine {
    fn new() -> ICacheLine {
        // The cache starts in a random state. In order to catch
        // missbehaving software we fill them with "trap" values
        ICacheLine {
            // Tag is 0, all line valid
            tag_valid: 0x0,
            // BREAK opcode
            //line: [Instruction(0xbadc0de5); 4],
            line: [Instruction(0); 4],
        }
    }

    /// Return the cacheline's tag
    fn tag(&self) -> u32 {
        self.tag_valid & 0xfffff000
    }

    /// Return the cacheline's first valid word
    fn valid_index(&self) -> u32 {
        // We store the valid bits in bits [4:2], this way we can just
        // mask the PC value in `set_tag_valid` without having to
        // shuffle the bits around
        (self.tag_valid >> 2) & 0x7
    }

    /// Set the cacheline's tag and valid bits. `pc` is the first
    /// valid PC in the cacheline.
    fn set_tag_valid(&mut self, pc: u32) {
        self.tag_valid =  pc & 0x7ffff00c;
    }

    /// Invalidate the entire cacheline by pushing the index out of
    /// range. Doesn't change the tag or contents of the line.
    fn invalidate(&mut self) {
        // Setting bit 4 means that the value returned by valid_index
        // will be in the range [4, 7] which is outside the valid
        // cacheline index range [0, 3].
        self.tag_valid |= 0x10;
    }

    fn instruction(&self, index: u32) -> Instruction {
        self.line[index as usize]
    }

    fn set_instruction(&mut self, index: u32, instruction: Instruction) {
        self.line[index as usize] = instruction;
    }
}

impl Default for ICacheLine {
    fn default() -> ICacheLine {
        ICacheLine::new()
    }
}

/// Serializable container for the cachelines
buffer!(struct ICacheLines([ICacheLine; 0x100]));

/// Value of the "Processor ID" register (Cop0r15). This is the value
/// returned by my SCPH-7502.
pub const PROCESSOR_ID: u32 = 0x00000002;

/// PlayStation CPU clock in Hz
pub const CPU_FREQ_HZ: u32 = 33_868_500;
