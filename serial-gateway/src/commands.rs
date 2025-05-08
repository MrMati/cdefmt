trait ValueAsBytes {
    fn as_bytes(&self) -> &[u8];
}

macro_rules! impl_value_as_bytes {
    ($($t:ty),*) => {
        $(impl ValueAsBytes for $t {
            fn as_bytes(&self) -> &[u8] {
                unsafe {
                    std::slice::from_raw_parts(
                        self as *const $t as *const u8,
                        std::mem::size_of::<$t>()
                    )
                }
            }
        })*
    };
}

impl_value_as_bytes!(u8, u16, u32, u64, i8, i16, i32, i64, f32);

macro_rules! define_var_enum_with_string_ids {
    (
        $(#[$enum_meta:meta])* // Enum attributes (derive, repr)
        $vis:vis enum $enum_name:ident {
            $(
                #[id = $string_id:literal] // Custom attribute for string ID
                $variant_name:ident($variant_ty:ty) = $discriminant:expr $(,)*
            )*
        }
    ) => {
        $(#[$enum_meta])*
        $vis enum $enum_name {
            $(
                $variant_name($variant_ty) = $discriminant,
            )*
        }

        impl $enum_name {
            $vis fn discriminant(&self) -> u8 {
                unsafe { *(self as *const Self as *const u8) }
            }

            $vis fn value_as_bytes(&self) -> &[u8] {
                match self {
                    $(
                        $enum_name::$variant_name(v) => v.as_bytes(),
                    )*
                }
            }

            $vis fn from_var_id_and_value(name_input: &str, value_str_input: &str) -> Option<Self> {
                let name_upper = name_input.to_uppercase(); // Case-insensitive matching for string ID
                match name_upper.as_str() {
                    $(
                        s if s == $string_id.to_uppercase() => {
                            // Parse the value_str_input directly into the variant's type
                            match value_str_input.parse::<$variant_ty>() {
                                Ok(parsed_value) => Some(Self::$variant_name(parsed_value)),
                                Err(_) => None, // Parsing failed for this type
                            }
                        }
                    )*
                    _ => None,
                }
            }
        }
    };
}

define_var_enum_with_string_ids! {
    #[derive(Debug)]
    #[repr(u8)]
    pub enum Variable {
        #[id = "PWM_A"] // Variable ID text protocol
        PwmA(u16) = 0x01, // ID used in serial transmission
        #[id = "PWM_B"]
        PwmB(u16) = 0x02,
        #[id = "F_TEST"]
        FTest(f32) = 0x03,
    }
}

pub fn process_udp_message(msg: &str) -> Option<Vec<u8>> {
    let msg = msg.trim();
    if !msg.starts_with('!') {
        return None;
    }

    let parts: Vec<&str> = msg[1..].split_whitespace().collect();
    if parts.len() != 2 {
        return None;
    }

    let (name, value_str) = (parts[0], parts[1]);

    Variable::from_var_id_and_value(name, value_str).map(create_variable_packet)
}

fn create_variable_packet(var: Variable) -> Vec<u8> {
    let data_size = var.value_as_bytes().len();
    let total_len = 1 + data_size; // var_id (1) + data

    let mut buffer = Vec::with_capacity(2 + total_len);
    buffer.extend_from_slice(&(total_len as u16).to_le_bytes());
    buffer.push(var.discriminant());
    buffer.extend_from_slice(var.value_as_bytes());
    buffer
}
