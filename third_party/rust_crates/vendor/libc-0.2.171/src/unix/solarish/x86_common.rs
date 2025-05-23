// AT_SUN_HWCAP
pub const AV_386_FPU: u32 = 0x00001;
pub const AV_386_TSC: u32 = 0x00002;
pub const AV_386_CX8: u32 = 0x00004;
pub const AV_386_SEP: u32 = 0x00008;
pub const AV_386_AMD_SYSC: u32 = 0x00010;
pub const AV_386_CMOV: u32 = 0x00020;
pub const AV_386_MMX: u32 = 0x00040;
pub const AV_386_AMD_MMX: u32 = 0x00080;
pub const AV_386_AMD_3DNow: u32 = 0x00100;
pub const AV_386_AMD_3DNowx: u32 = 0x00200;
pub const AV_386_FXSR: u32 = 0x00400;
pub const AV_386_SSE: u32 = 0x00800;
pub const AV_386_SSE2: u32 = 0x01000;
pub const AV_386_CX16: u32 = 0x10000;
pub const AV_386_AHF: u32 = 0x20000;
pub const AV_386_TSCP: u32 = 0x40000;
pub const AV_386_AMD_SSE4A: u32 = 0x80000;
pub const AV_386_POPCNT: u32 = 0x100000;
pub const AV_386_AMD_LZCNT: u32 = 0x200000;
pub const AV_386_SSSE3: u32 = 0x400000;
pub const AV_386_SSE4_1: u32 = 0x800000;
pub const AV_386_SSE4_2: u32 = 0x1000000;
pub const AV_386_MOVBE: u32 = 0x2000000;
pub const AV_386_AES: u32 = 0x4000000;
pub const AV_386_PCLMULQDQ: u32 = 0x8000000;
pub const AV_386_XSAVE: u32 = 0x10000000;
pub const AV_386_AVX: u32 = 0x20000000;
cfg_if! {
    if #[cfg(target_os = "illumos")] {
        pub const AV_386_VMX: u32 = 0x40000000;
        pub const AV_386_AMD_SVM: u32 = 0x80000000;
        // AT_SUN_HWCAP2
        pub const AV_386_2_F16C: u32 = 0x00000001;
        pub const AV_386_2_RDRAND: u32 = 0x00000002;
        pub const AV_386_2_BMI1: u32 = 0x00000004;
        pub const AV_386_2_BMI2: u32 = 0x00000008;
        pub const AV_386_2_FMA: u32 = 0x00000010;
        pub const AV_386_2_AVX2: u32 = 0x00000020;
        pub const AV_386_2_ADX: u32 = 0x00000040;
        pub const AV_386_2_RDSEED: u32 = 0x00000080;
        pub const AV_386_2_AVX512F: u32 = 0x00000100;
        pub const AV_386_2_AVX512DQ: u32 = 0x00000200;
        pub const AV_386_2_AVX512IFMA: u32 = 0x00000400;
        pub const AV_386_2_AVX512PF: u32 = 0x00000800;
        pub const AV_386_2_AVX512ER: u32 = 0x00001000;
        pub const AV_386_2_AVX512CD: u32 = 0x00002000;
        pub const AV_386_2_AVX512BW: u32 = 0x00004000;
        pub const AV_386_2_AVX512VL: u32 = 0x00008000;
        pub const AV_386_2_AVX512VBMI: u32 = 0x00010000;
        pub const AV_386_2_AVX512VPOPCDQ: u32 = 0x00020000;
        pub const AV_386_2_AVX512_4NNIW: u32 = 0x00040000;
        pub const AV_386_2_AVX512_4FMAPS: u32 = 0x00080000;
        pub const AV_386_2_SHA: u32 = 0x00100000;
        pub const AV_386_2_FSGSBASE: u32 = 0x00200000;
        pub const AV_386_2_CLFLUSHOPT: u32 = 0x00400000;
        pub const AV_386_2_CLWB: u32 = 0x00800000;
        pub const AV_386_2_MONITORX: u32 = 0x01000000;
        pub const AV_386_2_CLZERO: u32 = 0x02000000;
        pub const AV_386_2_AVX512_VNNI: u32 = 0x04000000;
        pub const AV_386_2_VPCLMULQDQ: u32 = 0x08000000;
        pub const AV_386_2_VAES: u32 = 0x10000000;
        // AT_SUN_FPTYPE
        pub const AT_386_FPINFO_NONE: u32 = 0;
        pub const AT_386_FPINFO_FXSAVE: u32 = 1;
        pub const AT_386_FPINFO_XSAVE: u32 = 2;
        pub const AT_386_FPINFO_XSAVE_AMD: u32 = 3;
    }
}
