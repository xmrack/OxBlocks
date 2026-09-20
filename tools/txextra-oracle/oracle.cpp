#include <cstdio>
#include <cstring>
#include <string>
#include <vector>
#include <iostream>
#include <typeinfo>
#include <boost/variant/get.hpp>
#include "serialization/serialization.h"
#include "serialization/binary_archive.h"
#include "serialization/variant.h"
#include "serialization/string.h"
#include "serialization/containers.h"
#include "serialization/crypto.h"
#include "cryptonote_basic/tx_extra.h"

using namespace cryptonote;

// Verbatim copy of cryptonote::parse_tx_extra (src/cryptonote_basic/cryptonote_format_utils.cpp:564)
// with only the logging macros replaced.
static bool my_parse_tx_extra(const std::vector<uint8_t>& tx_extra, std::vector<tx_extra_field>& tx_extra_fields)
{
  tx_extra_fields.clear();
  if(tx_extra.empty()) return true;
  binary_archive<false> ar{epee::to_span(tx_extra)};
  do
  {
    tx_extra_field field;
    bool r = ::do_serialize(ar, field);
    if (!r) { return false; }
    tx_extra_fields.push_back(field);
  } while (!ar.eof());
  if (!::serialization::check_stream_state(ar)) { return false; }
  return true;
}

static std::string hx(const std::string &s){ static const char*H="0123456789abcdef"; std::string o; for(unsigned char c: s){o+=H[c>>4];o+=H[c&15];} return o; }
static std::string hxp(const void*p, size_t n){ return hx(std::string((const char*)p,n)); }

int main(int argc, char** argv)
{
  if (argc < 2) { fprintf(stderr,"usage: oracle <hex>\n"); return 1; }
  std::string h = argv[1];
  std::vector<uint8_t> extra;
  for (size_t i=0;i+1<h.size();i+=2) extra.push_back((uint8_t)strtol(h.substr(i,2).c_str(),nullptr,16));
  std::vector<tx_extra_field> fields;
  bool ok = my_parse_tx_extra(extra, fields);
  printf("parse=%s nfields=%zu\n", ok?"TRUE":"FALSE", fields.size());
  for (size_t n=0;n<fields.size();n++)
  {
    printf("  [%zu] ", n);
    if (typeid(tx_extra_padding)==fields[n].type()) printf("PADDING size=%zu", boost::get<tx_extra_padding>(fields[n]).size);
    else if (typeid(tx_extra_pub_key)==fields[n].type()) printf("PUBKEY %s", hxp(&boost::get<tx_extra_pub_key>(fields[n]).pub_key,32).c_str());
    else if (typeid(tx_extra_nonce)==fields[n].type()) { auto &s=boost::get<tx_extra_nonce>(fields[n]).nonce; printf("NONCE len=%zu data=%s", s.size(), hx(s).c_str()); }
    else if (typeid(tx_extra_merge_mining_tag)==fields[n].type()) { auto &m=boost::get<tx_extra_merge_mining_tag>(fields[n]); printf("MM depth=%llu root=%s",(unsigned long long)m.depth, hxp(&m.merkle_root,32).c_str()); }
    else if (typeid(tx_extra_additional_pub_keys)==fields[n].type()) { auto &v=boost::get<tx_extra_additional_pub_keys>(fields[n]).data; printf("ADDL n=%zu:",v.size()); for(auto&k:v) printf(" %s", hxp(&k,32).c_str()); }
    else if (typeid(tx_extra_mysterious_minergate)==fields[n].type()) { auto &s=boost::get<tx_extra_mysterious_minergate>(fields[n]).data; printf("MINERGATE len=%zu data=%s", s.size(), hx(s).c_str()); }
    printf("\n");
  }
  return 0;
}
