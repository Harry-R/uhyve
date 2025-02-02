//! This file contains the entry point to the Hypervisor. The Uhyve utilizes KVM to
//! create a Virtual Machine and load the kernel.

use crate::consts::*;
use crate::linux::vcpu::*;
use crate::linux::virtio::*;
use crate::linux::KVM;
use crate::shared_queue::*;
use crate::vm::HypervisorResult;
use crate::vm::{BootInfo, Parameter, Vm};
use kvm_bindings::*;
use kvm_ioctls::VmFd;
use log::debug;
use nix::sys::mman::*;
use std::fmt;
use std::hint;
use std::mem;
use std::net::Ipv4Addr;
use std::os::raw::c_void;
use std::path::Path;
use std::path::PathBuf;
use std::ptr;
use std::ptr::{read_volatile, write_volatile};
use std::str::FromStr;
use std::sync::mpsc::sync_channel;
use std::sync::{Arc, Mutex};
use std::thread;
use tun_tap::{Iface, Mode};
use vmm_sys_util::eventfd::EventFd;

const KVM_32BIT_MAX_MEM_SIZE: usize = 1 << 32;
const KVM_32BIT_GAP_SIZE: usize = 768 << 20;
const KVM_32BIT_GAP_START: usize = KVM_32BIT_MAX_MEM_SIZE - KVM_32BIT_GAP_SIZE;

#[derive(Debug)]
struct UhyveNetwork {
	#[allow(dead_code)]
	reader: std::thread::JoinHandle<()>,
	#[allow(dead_code)]
	writer: std::thread::JoinHandle<()>,
	tx: std::sync::mpsc::SyncSender<usize>,
}

impl UhyveNetwork {
	pub fn new(evtfd: EventFd, name: String, start: usize) -> Self {
		let iface = Arc::new(
			Iface::without_packet_info(&name, Mode::Tap).expect("Unable to creat TUN/TAP device"),
		);

		let iface_writer = Arc::clone(&iface);
		let iface_reader = Arc::clone(&iface);
		let (tx, rx) = sync_channel(1);

		let writer = thread::spawn(move || {
			let tx_queue = unsafe {
				#[allow(clippy::cast_ptr_alignment)]
				&mut *((start + align_up!(mem::size_of::<SharedQueue>(), 64)) as *mut u8
					as *mut SharedQueue)
			};
			tx_queue.init();

			loop {
				let _value = rx.recv().expect("Failed to read from channel");

				let written = unsafe { read_volatile(&tx_queue.written) };
				let read = unsafe { read_volatile(&tx_queue.read) };
				let distance = written - read;

				if distance > 0 {
					let idx = read % UHYVE_QUEUE_SIZE;
					let len = unsafe { read_volatile(&tx_queue.inner[idx].len) } as usize;
					let _ = iface_writer
						.send(&tx_queue.inner[idx].data[0..len])
						.expect("Send on TUN/TAP device failed!");

					unsafe { write_volatile(&mut tx_queue.read, read + 1) };
				}
			}
		});

		let reader = thread::spawn(move || {
			let rx_queue = unsafe {
				#[allow(clippy::cast_ptr_alignment)]
				&mut *(start as *mut u8 as *mut SharedQueue)
			};
			rx_queue.init();

			loop {
				let written = unsafe { read_volatile(&rx_queue.written) };
				let read = unsafe { read_volatile(&rx_queue.read) };
				let distance = written - read;

				if distance < UHYVE_QUEUE_SIZE {
					let idx = written % UHYVE_QUEUE_SIZE;
					unsafe {
						write_volatile(
							&mut rx_queue.inner[idx].len,
							iface_reader
								.recv(&mut rx_queue.inner[idx].data)
								.expect("Receive on TUN/TAP device failed!")
								.try_into()
								.unwrap(),
						);
						write_volatile(&mut rx_queue.written, written + 1);
					}

					evtfd.write(1).expect("Unable to trigger interrupt");
				} else {
					hint::spin_loop();
				}
			}
		});

		UhyveNetwork { reader, writer, tx }
	}
}

impl Drop for UhyveNetwork {
	fn drop(&mut self) {
		debug!("Dropping network interface!");
	}
}

pub struct Uhyve {
	vm: VmFd,
	offset: u64,
	entry_point: u64,
	mem: MmapMemory,
	num_cpus: u32,
	path: PathBuf,
	boot_info: *const BootInfo,
	verbose: bool,
	ip: Option<Ipv4Addr>,
	gateway: Option<Ipv4Addr>,
	mask: Option<Ipv4Addr>,
	uhyve_device: Option<UhyveNetwork>,
	virtio_device: Arc<Mutex<VirtioNetPciDevice>>,
	pub(super) gdb_port: Option<u16>,
}

impl fmt::Debug for Uhyve {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Uhyve")
			.field("entry_point", &self.entry_point)
			.field("mem", &self.mem)
			.field("num_cpus", &self.num_cpus)
			.field("path", &self.path)
			.field("boot_info", &self.boot_info)
			.field("verbose", &self.verbose)
			.field("ip", &self.ip)
			.field("gateway", &self.gateway)
			.field("mask", &self.mask)
			.field("uhyve_device", &self.uhyve_device)
			.field("virtio_device", &self.virtio_device)
			.finish()
	}
}

impl Uhyve {
	pub fn new(kernel_path: PathBuf, specs: &Parameter<'_>) -> HypervisorResult<Uhyve> {
		// parse string to get IP address
		let ip_addr = specs
			.ip
			.as_ref()
			.map(|addr_str| Ipv4Addr::from_str(addr_str).expect("Unable to parse ip address"));

		// parse string to get gateway address
		let gw_addr = specs
			.gateway
			.as_ref()
			.map(|addr_str| Ipv4Addr::from_str(addr_str).expect("Unable to parse gateway address"));

		// parse string to get gateway address
		let mask = specs
			.mask
			.as_ref()
			.map(|addr_str| Ipv4Addr::from_str(addr_str).expect("Unable to parse network parse"));

		let vm = KVM.create_vm()?;

		let mem = MmapMemory::new(0, specs.mem_size, 0, specs.hugepage, specs.mergeable);

		let sz = if specs.mem_size < KVM_32BIT_GAP_START {
			specs.mem_size
		} else {
			KVM_32BIT_GAP_START
		};

		// create virtio interface
		let virtio_device = Arc::new(Mutex::new(VirtioNetPciDevice::new()));

		let kvm_mem = kvm_userspace_memory_region {
			slot: 0,
			flags: mem.flags,
			memory_size: sz as u64,
			guest_phys_addr: mem.guest_address as u64,
			userspace_addr: mem.host_address as u64,
		};

		unsafe { vm.set_user_memory_region(kvm_mem) }?;

		if specs.mem_size > KVM_32BIT_GAP_START + KVM_32BIT_GAP_SIZE {
			let kvm_mem = kvm_userspace_memory_region {
				slot: 1,
				flags: mem.flags,
				memory_size: (specs.mem_size - KVM_32BIT_GAP_START - KVM_32BIT_GAP_SIZE) as u64,
				guest_phys_addr: (mem.guest_address + KVM_32BIT_GAP_START + KVM_32BIT_GAP_SIZE)
					as u64,
				userspace_addr: (mem.host_address + KVM_32BIT_GAP_START + KVM_32BIT_GAP_SIZE)
					as u64,
			};

			unsafe { vm.set_user_memory_region(kvm_mem) }?;
		}

		debug!("Initialize interrupt controller");

		// create basic interrupt controller
		vm.create_irq_chip()?;

		// enable x2APIC support
		let mut cap: kvm_enable_cap = kvm_bindings::kvm_enable_cap {
			cap: KVM_CAP_X2APIC_API,
			flags: 0,
			..Default::default()
		};
		cap.args[0] =
			(KVM_X2APIC_API_USE_32BIT_IDS | KVM_X2APIC_API_DISABLE_BROADCAST_QUIRK).into();
		vm.enable_cap(&cap)
			.expect("Unable to enable x2apic support");

		// currently, we support only system, which provides the
		// cpu feature TSC_DEADLINE
		let mut cap: kvm_enable_cap = kvm_bindings::kvm_enable_cap {
			cap: KVM_CAP_TSC_DEADLINE_TIMER,
			..Default::default()
		};
		cap.args[0] = 0;
		vm.enable_cap(&cap)
			.expect_err("Processor feature `tsc deadline` isn't supported!");

		let cap: kvm_enable_cap = kvm_bindings::kvm_enable_cap {
			cap: KVM_CAP_IRQFD,
			..Default::default()
		};
		vm.enable_cap(&cap)
			.expect_err("The support of KVM_CAP_IRQFD is currently required");

		let mut cap: kvm_enable_cap = kvm_bindings::kvm_enable_cap {
			cap: KVM_CAP_X86_DISABLE_EXITS,
			flags: 0,
			..Default::default()
		};
		cap.args[0] =
			(KVM_X86_DISABLE_EXITS_PAUSE | KVM_X86_DISABLE_EXITS_MWAIT | KVM_X86_DISABLE_EXITS_HLT)
				.into();
		vm.enable_cap(&cap)
			.expect("Unable to disable exists due pause instructions");

		let evtfd = EventFd::new(0).unwrap();
		vm.register_irqfd(&evtfd, UHYVE_IRQ_NET)?;
		// create TUN/TAP device
		let uhyve_device = match &specs.nic {
			Some(nic) => {
				debug!("Initialize network interface");
				Some(UhyveNetwork::new(
					evtfd,
					nic.to_string(),
					mem.host_address + SHAREDQUEUE_START,
				))
			}
			_ => None,
		};

		assert!(
			specs.gdbport.is_none() || specs.num_cpus == 1,
			"gdbstub is only supported with one CPU"
		);

		let hyve = Uhyve {
			vm,
			offset: 0,
			entry_point: 0,
			mem,
			num_cpus: specs.num_cpus,
			path: kernel_path,
			boot_info: ptr::null(),
			verbose: specs.verbose,
			ip: ip_addr,
			gateway: gw_addr,
			mask,
			uhyve_device,
			virtio_device,
			gdb_port: specs.gdbport,
		};

		hyve.init_guest_mem();

		Ok(hyve)
	}
}

impl Vm for Uhyve {
	fn verbose(&self) -> bool {
		self.verbose
	}

	fn set_offset(&mut self, offset: u64) {
		self.offset = offset;
	}

	fn get_offset(&self) -> u64 {
		self.offset
	}

	fn set_entry_point(&mut self, entry: u64) {
		self.entry_point = entry;
	}

	fn get_entry_point(&self) -> u64 {
		self.entry_point
	}

	fn get_ip(&self) -> Option<Ipv4Addr> {
		self.ip
	}

	fn get_gateway(&self) -> Option<Ipv4Addr> {
		self.gateway
	}

	fn get_mask(&self) -> Option<Ipv4Addr> {
		self.mask
	}

	fn num_cpus(&self) -> u32 {
		self.num_cpus
	}

	fn guest_mem(&self) -> (*mut u8, usize) {
		(self.mem.host_address as *mut u8, self.mem.memory_size)
	}

	fn kernel_path(&self) -> &Path {
		self.path.as_path()
	}

	fn create_cpu(&self, id: u32) -> HypervisorResult<UhyveCPU> {
		let vm_start = self.mem.host_address as usize;
		let tx = self.uhyve_device.as_ref().map(|dev| dev.tx.clone());

		Ok(UhyveCPU::new(
			id,
			self.path.clone(),
			self.vm.create_vcpu(id.try_into().unwrap())?,
			vm_start,
			tx,
			self.virtio_device.clone(),
		))
	}

	fn set_boot_info(&mut self, header: *const BootInfo) {
		self.boot_info = header;
	}

	fn cpu_online(&self) -> u32 {
		if self.boot_info.is_null() {
			0
		} else {
			unsafe { read_volatile(&(*self.boot_info).cpu_online) }
		}
	}
}

impl Drop for Uhyve {
	fn drop(&mut self) {
		debug!("Drop virtual machine");
	}
}

// TODO: Investigate soundness
// https://github.com/hermitcore/uhyve/issues/229
#[allow(clippy::non_send_fields_in_send_ty)]
unsafe impl Send for Uhyve {}
unsafe impl Sync for Uhyve {}

#[derive(Debug)]
struct MmapMemory {
	flags: u32,
	memory_size: usize,
	guest_address: usize,
	host_address: usize,
}

impl MmapMemory {
	pub fn new(
		flags: u32,
		memory_size: usize,
		guest_address: u64,
		huge_pages: bool,
		mergeable: bool,
	) -> MmapMemory {
		let host_address = unsafe {
			mmap(
				std::ptr::null_mut(),
				memory_size,
				ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
				MapFlags::MAP_PRIVATE | MapFlags::MAP_ANONYMOUS | MapFlags::MAP_NORESERVE,
				-1,
				0,
			)
			.expect("mmap failed")
		};

		if mergeable {
			debug!("Enable kernel feature to merge same pages");
			unsafe {
				madvise(host_address, memory_size, MmapAdvise::MADV_MERGEABLE).unwrap();
			}
		}

		if huge_pages {
			debug!("Uhyve uses huge pages");
			unsafe {
				madvise(host_address, memory_size, MmapAdvise::MADV_HUGEPAGE).unwrap();
			}
		}

		MmapMemory {
			flags,
			memory_size,
			guest_address: guest_address as usize,
			host_address: host_address as usize,
		}
	}

	#[allow(dead_code)]
	fn as_slice_mut(&mut self) -> &mut [u8] {
		unsafe { std::slice::from_raw_parts_mut(self.host_address as *mut u8, self.memory_size) }
	}
}

impl Drop for MmapMemory {
	fn drop(&mut self) {
		if self.memory_size > 0 {
			unsafe {
				munmap(self.host_address as *mut c_void, self.memory_size).unwrap();
			}
		}
	}
}
