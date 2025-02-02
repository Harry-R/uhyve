#![warn(rust_2018_idioms)]

#[macro_use]
extern crate log;
#[macro_use]
extern crate clap;

#[cfg(feature = "instrument")]
extern crate rftrace_frontend;

use std::collections::HashSet;
use std::env;
use std::path::PathBuf;
use std::str::FromStr;

use uhyvelib::utils;
use uhyvelib::vm;
use uhyvelib::Uhyve;

use byte_unit::Byte;
use clap::{App, Arg};
#[cfg(feature = "instrument")]
use rftrace_frontend::Events;

const MINIMAL_GUEST_SIZE: usize = 16 * 1024 * 1024;
const DEFAULT_GUEST_SIZE: usize = 64 * 1024 * 1024;

#[cfg(feature = "instrument")]
static mut EVENTS: Option<&mut Events> = None;

#[cfg(feature = "instrument")]
extern "C" fn dump_trace() {
	unsafe {
		if let Some(ref mut e) = EVENTS {
			rftrace_frontend::dump_full_uftrace(&mut *e, "uhyve_trace", "uhyve", true)
				.expect("Saving trace failed");
		}
	}
}

// Note that we end main with `std::process::exit` to set the return value and
// as a result destructors are not run and cleanup may not happen.
fn main() {
	#[cfg(feature = "instrument")]
	{
		let events = rftrace_frontend::init(1000000, true);
		rftrace_frontend::enable();

		unsafe {
			EVENTS = Some(events);
			libc::atexit(dump_trace);
		}
	}

	env_logger::init();

	let matches = App::new("uhyve")
		.version(crate_version!())
		.setting(clap::AppSettings::TrailingVarArg)
		.setting(clap::AppSettings::AllowLeadingHyphen)
		.author(crate_authors!("\n"))
		.about("A minimal hypervisor for RustyHermit")
		.arg(
			Arg::with_name("VERBOSE")
				.short("v")
				.long("verbose")
				.help("Print also kernel messages"),
		)
		.arg(
			Arg::with_name("DISABLE_HUGEPAGE")
				.long("disable-hugepages")
				.help("Disable the usage of huge pages"),
		)
		.arg(
			Arg::with_name("MERGEABLE")
				.long("mergeable")
				.help("Enable kernel feature to merge same pages"),
		)
		.arg(
			Arg::with_name("MEM")
				.short("m")
				.long("memsize")
				.value_name("MEM")
				.help("Memory size of the guest")
				.takes_value(true)
				.env("HERMIT_MEM"),
		)
		.arg(
			Arg::with_name("CPUS")
				.short("c")
				.long("cpus")
				.value_name("CPUS")
				.help("Number of guest processors")
				.takes_value(true)
				.env("HERMIT_CPUS"),
		)
		.arg(
			Arg::with_name("CPU_AFFINITY")
				.short("a")
				.long("affinity")
				.value_name("cpulist")
				.help("CPU Affinity of guest CPUs on Host")
				.long_help(
					"A list of CPUs delimited by commas onto which
					 the virtual CPUs should be bound. This may improve 
					performance.
					",
				),
		)
		.arg(
			Arg::with_name("GDB_PORT")
				.short("s")
				.long("gdb_port")
				.value_name("GDB_PORT")
				.help("Enables GDB-Stub on given port")
				.takes_value(true)
				.env("HERMIT_GDB_PORT"),
		)
		.arg(
			Arg::with_name("NETIF")
				.long("nic")
				.value_name("NETIF")
				.help("Name of the network interface")
				.takes_value(true)
				.env("HERMIT_NETIF"),
		)
		/*.arg(
			Arg::with_name("IP")
				.long("ip")
				.value_name("IP")
				.help("IP address of the guest")
				.takes_value(true)
				.env("HERMIT_IP"),
		)
		.arg(
			Arg::with_name("GATEWAY")
				.long("gateway")
				.value_name("GATEWAY")
				.help("Gateway address")
				.takes_value(true)
				.env("HERMIT_GATEWAY"),
		)
		.arg(
			Arg::with_name("MASK")
				.long("mask")
				.value_name("MASK")
				.help("Network mask")
				.takes_value(true)
				.env("HERMIT_MASK"),
		)
		.arg(
			Arg::with_name("MAC")
				.long("mac")
				.value_name("MAC")
				.help("MAC address of the network interface")
				.takes_value(true)
				.env("HERMIT_MASK"),
		)*/
		.arg(
			Arg::with_name("KERNEL")
				.help("Sets path to the kernel")
				.required(true)
				.index(1),
		)
		.arg(
			Arg::with_name("ARGUMENTS")
				.help("Arguments of the unikernel")
				.required(false)
				.multiple(true)
				.max_values(255),
		)
		.get_matches();

	let path = PathBuf::from_str(
		matches
			.value_of("KERNEL")
			.expect("Expect path to the kernel!"),
	)
	.expect("Invalid kernel path");
	let mem_size: usize = matches
		.value_of("MEM")
		.map(|s| {
			let mem = Byte::from_str(s)
				.expect("Invalid MEM specified")
				.get_bytes()
				.try_into()
				.unwrap();
			if mem < MINIMAL_GUEST_SIZE {
				warn!(
					"Resize guest memory to {}",
					Byte::from_bytes(MINIMAL_GUEST_SIZE.try_into().unwrap())
				);
				MINIMAL_GUEST_SIZE
			} else {
				mem
			}
		})
		.unwrap_or(DEFAULT_GUEST_SIZE);
	let num_cpus = matches
		.value_of("CPUS")
		.map(|cpus| cpus.parse().ok())
		.flatten()
		.unwrap_or(1);

	let cpu_affinity = matches.values_of("CPU_AFFINITY").map(|affinity| {
		let parsed_affinity = utils::parse_ranges(affinity)
			.collect::<Result<HashSet<_>, _>>()
			.expect("Invalid parameters passed for CPU_AFFINITY");

		// According to https://github.com/Elzair/core_affinity_rs/issues/3
		// on linux this gives a list of CPUs the process is allowed to run on
		// (as opposed to all CPUs available on the system as the docs suggest)
		let core_ids = core_affinity::get_core_ids()
			.expect("Dependency core_affinity failed to find any available CPUs")
			.into_iter()
			.filter(|core_id| parsed_affinity.contains(&core_id.id))
			.collect::<Vec<_>>();
		assert_eq!(core_ids.len(), num_cpus as usize);
		core_ids
	});

	let ip = None; //matches.value_of("IP").or(None);
	let gateway = None; // matches.value_of("GATEWAY").or(None);
	let mask = None; //matches.value_of("MASK").or(None);
	let nic = None; //matches.value_of("NETIF").or(None);

	let mut mergeable = envmnt::is_or("HERMIT_MERGEABLE", false);
	if matches.is_present("MERGEABLE") {
		mergeable = true;
	}
	// per default we use huge page to improve the performance,
	// if the kernel supports transparent hugepages
	let hugepage_default = utils::transparent_hugepages_available().unwrap_or(true);
	info!("Default hugepages set to: {}", hugepage_default);
	// HERMIT_HUGEPAGES overrides the default we detected.
	let mut hugepage = envmnt::is_or("HERMIT_HUGEPAGE", hugepage_default);
	if matches.is_present("DISABLE_HUGEPAGE") {
		hugepage = false;
	}
	let mut verbose = envmnt::is_or("HERMIT_VERBOSE", false);
	if matches.is_present("VERBOSE") {
		verbose = true;
	}
	let gdbport = matches
		.value_of("GDB_PORT")
		.map(|p| p.parse::<u16>().expect("Could not parse gdb port"))
		.or_else(|| {
			env::var("HERMIT_GDB_PORT")
				.ok()
				.map(|p| p.parse::<u16>().expect("Could not parse gdb port"))
		});

	let params = vm::Parameter {
		mem_size,
		num_cpus,
		verbose,
		hugepage,
		mergeable,
		ip,
		gateway,
		mask,
		nic,
		gdbport,
	};

	let code = Uhyve::new(path, &params)
		.expect("Unable to create VM! Is the hypervisor interface (e.g. KVM) activated?")
		.run(cpu_affinity);
	std::process::exit(code);
}
