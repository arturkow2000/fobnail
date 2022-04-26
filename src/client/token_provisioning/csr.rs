use alloc::vec::Vec;
use der::{
    asn1::{BitString, SetOfVec, UIntBytes},
    Any, Encodable, Sequence, Tag,
};
use pkcs10::{CertReq, CertReqInfo};
use rsa::{PublicKeyParts, RsaPrivateKey, RsaPublicKey};
use sha2::Digest;
use x501::{attr::AttributeTypeAndValue, name::DistinguishedName};
use x509::{
    AlgorithmIdentifier, ObjectIdentifier, SubjectPublicKeyInfo, PKIX_AT_ORGANIZATIONNAME,
    PKIX_AT_SERIALNUMBER,
};

/// Encode RSA public key in ASN.1
fn rsa_asn1_encode(key: &RsaPublicKey) -> Result<Vec<u8>, ()> {
    #[derive(Sequence)]
    struct RsaPublicKey<'a> {
        pub n: UIntBytes<'a>,
        pub e: UIntBytes<'a>,
    }

    let n = key.n().to_bytes_be();
    let e = key.e().to_bytes_be();

    let mut buf = vec![0u8; n.len() + e.len() + 32];
    let mut encoder = der::Encoder::new(&mut buf);
    encoder
        .encode(&RsaPublicKey {
            n: UIntBytes::new(&n).map_err(|e| error!("{}", e))?,
            e: UIntBytes::new(&e).map_err(|e| error!("{}", e))?,
        })
        .map_err(|e| error!("{}", e))?;
    let b = encoder.finish().map_err(|e| error!("{}", e))?;
    let encoded_len = b.len();

    buf.truncate(encoded_len);
    Ok(buf)
}

fn asn1_encode_sign<'r, T: Encodable>(
    signing_key: &RsaPrivateKey,
    object: &T,
    buf: &'r mut [u8],
) -> Result<(&'r [u8], Vec<u8>), ()> {
    let mut encoder = der::Encoder::new(buf);
    encoder.encode(object).map_err(|e| error!("{}", e))?;
    let encoded = encoder.finish().map_err(|e| error!("{}", e))?;

    let mut hasher = sha2::Sha256::new();
    hasher.update(encoded);
    let hash = hasher.finalize();

    let signature = signing_key
        .sign(
            rsa::PaddingScheme::PKCS1v15Sign {
                hash: Some(rsa::Hash::SHA2_256),
            },
            &hash,
        )
        .map_err(|e| error!("Failed to sign ASN.1 object: {}", e))?;

    Ok((encoded, signature))
}

pub fn make_csr(
    key_priv: RsaPrivateKey,
    key_pub: RsaPublicKey,
    device_id: u64,
) -> Result<Vec<u8>, ()> {
    let device_id_str = format!("{:X}", device_id);

    let key_pub_asn1 =
        rsa_asn1_encode(&key_pub).map_err(|()| error!("Failed to ASN.1 encode RSA public key"))?;

    let organization = AttributeTypeAndValue {
        oid: PKIX_AT_ORGANIZATIONNAME,
        value: Any::new(Tag::Utf8String, b"Fobnail").unwrap(),
    };

    let serial_number = AttributeTypeAndValue {
        oid: PKIX_AT_SERIALNUMBER,
        value: Any::new(Tag::PrintableString, device_id_str.as_bytes()).unwrap(),
    };

    let mut buf = Vec::new();
    buf.resize(
        // Buffer must hold modulus + signature (which size equals to modulus size) + exponent
        key_pub.n().bits() / 8 * 2 + key_pub.e().bits() / 8
        // Add some space for other data
        + 128,
        0,
    );

    // Construct subject DN. Follow OpenSSL behaviour - put each RDN in a
    // separate set:
    //
    // SEQUENCE
    // SET
    //  SEQUENCE
    //   OBJECT            :organizationName
    //   UTF8STRING        :Fobnail
    // SET
    //  SEQUENCE
    //   OBJECT            :serialNumber
    //   UTF8STRING        :S534081NQW10

    let mut subject = DistinguishedName::new();
    subject.push(vec![organization].try_into().unwrap());
    subject.push(vec![serial_number].try_into().unwrap());

    // This part of CSR must be signed.
    let info = CertReqInfo {
        version: pkcs10::Version::V1,
        subject,
        attributes: SetOfVec::new(),
        public_key: SubjectPublicKeyInfo {
            algorithm: AlgorithmIdentifier {
                // rsaEncryption
                oid: ObjectIdentifier::new("1.2.840.113549.1.1.1"),
                parameters: None,
            },
            subject_public_key: &key_pub_asn1,
        },
    };

    let (_encoded_info, signature) =
        asn1_encode_sign(&key_priv, &info, &mut buf).map_err(|()| error!("CSR signing failed"))?;

    let req = CertReq {
        info,
        algorithm: AlgorithmIdentifier {
            // sha256WithRSAEncryption
            oid: ObjectIdentifier::new("1.2.840.113549.1.1.11"),
            parameters: None,
        },
        signature: BitString::from_bytes(&signature).unwrap(),
    };

    let mut encoder = der::Encoder::new(&mut buf);
    encoder.encode(&req).unwrap();
    let encoded_data_len = encoder.finish().unwrap().len();

    buf.truncate(encoded_data_len);
    Ok(buf)
}
